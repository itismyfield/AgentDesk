use super::*;
use crate::test_support::wait::{WAIT_CAP, await_within};
use std::io::Write;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
fn capture(logs: &Logs) -> impl tracing::Subscriber + use<> {
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .finish()
}
fn request() -> DecisionRequest {
    DecisionRequest {
        scope: Scope {
            feature: Feature::HostAssignment,
            repository: None,
        },
        template_version: "fixture-v1".into(),
        input_text: "Synthetic host loads".into(),
        question: Question::Choice {
            instructions: "Choose safely".into(),
            options: vec![
                ("host-a".into(), "A".into()),
                ("host-b".into(), "B".into()),
                ("hold".into(), "Wait".into()),
            ],
        },
    }
}
fn fixture() -> String {
    let data: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/decision_gate/comment_shape.json"
    ))
    .unwrap();
    data["choice"].to_string()
}
fn gate(endpoint: String, source: PolicySource) -> DecisionGate {
    let mut gate = DecisionGate::new(
        source,
        SecretString::new("fixture-only-memory-credential".into()),
    );
    gate.endpoint = endpoint;
    gate
}
async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let size = stream.read(&mut chunk).await.unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&chunk[..size]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]);
            let length: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap();
            if bytes.len() >= end + 4 + length {
                assert!(head.starts_with("POST /v1beta/decisions HTTP/1.1"));
                assert!(
                    head.to_ascii_lowercase()
                        .contains("content-type: application/json")
                );
                assert!(head.to_ascii_lowercase().contains("x-goog-api-key:"));
                return String::from_utf8(bytes[end + 4..].to_vec()).unwrap();
            }
        }
    }
}
async fn server(
    status: u16,
    body: String,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1beta/decisions", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
            let body = body.clone();
            tokio::spawn(async move {
                let data: serde_json::Value =
                    serde_json::from_str(&read_request(&mut stream).await).unwrap();
                assert_eq!(data["model"], "gemini-decision-1");
                assert!(data["questions"]["q"].is_object());
                let location = if status == 302 {
                    "Location: /v1beta/decisions\r\n"
                } else {
                    ""
                };
                let reply = format!(
                    "HTTP/1.1 {status} fixture\r\n{location}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
            });
        }
    });
    (endpoint, calls, handle)
}
async fn stop(server: tokio::task::JoinHandle<()>) {
    server.abort();
    assert!(
        await_within(server, WAIT_CAP)
            .await
            .unwrap_err()
            .is_cancelled()
    );
}

#[tokio::test]
async fn missing_key_ten_attempts_send_zero_http_and_emit_zero_logs() {
    let (endpoint, calls, server) = server(200, fixture()).await;
    let mut gate = DecisionGate::new(Arc::new(|| Some(DecisionGateConfig::default())), None);
    gate.endpoint = endpoint;
    let logs = Logs::default();
    let _capture = tracing::subscriber::set_default(capture(&logs));
    for _ in 0..10 {
        assert_eq!(
            gate.availability(&request().scope),
            Err(OffReason::MissingKey)
        );
        assert_eq!(
            gate.decide(&request()).await.result,
            DecisionResult::Off(OffReason::MissingKey)
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(logs.0.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    stop(server).await;
}

#[tokio::test]
async fn http_answers_and_failure_classes_are_sanitized_without_retries() {
    for (status, body, expected) in [
        (200, fixture(), None),
        (
            500,
            "fixture-only-memory-credential".into(),
            Some(NoAnswerReason::ProviderError),
        ),
        (401, String::new(), Some(NoAnswerReason::Authentication)),
        (403, String::new(), Some(NoAnswerReason::Authentication)),
        (429, String::new(), Some(NoAnswerReason::RateLimited)),
        (302, String::new(), Some(NoAnswerReason::ProviderError)),
        (
            200,
            "not JSON fixture-only-memory-credential".into(),
            Some(NoAnswerReason::InvalidResponse),
        ),
        (
            200,
            r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#.into(),
            Some(NoAnswerReason::Blocked),
        ),
        (
            200,
            r#"{"answers":{}}"#.into(),
            Some(NoAnswerReason::MissingAnswer),
        ),
    ] {
        let (endpoint, calls, server) = server(status, body).await;
        let gate = gate(endpoint, Arc::new(|| Some(DecisionGateConfig::default())));
        let logs = Logs::default();
        let _capture = tracing::subscriber::set_default(capture(&logs));
        let attempt = await_within(gate.decide(&request()), WAIT_CAP).await;
        if let Some(reason) = expected {
            assert_eq!(attempt.result, DecisionResult::NoAnswer(reason));
            assert_eq!(attempt.observation.input_tokens, None);
        } else {
            assert!(matches!(attempt.result, DecisionResult::Answer { .. }));
            assert_eq!(attempt.observation.input_tokens, Some(209));
            assert_eq!(attempt.observation.template_version, "fixture-v1");
        }
        assert!(!format!("{attempt:?}").contains("fixture-only-memory-credential"));
        assert!(
            !String::from_utf8_lossy(&logs.0.lock().unwrap_or_else(|e| e.into_inner()))
                .contains("fixture-only-memory-credential")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        stop(server).await;
    }
}

#[tokio::test]
async fn feature_deadlines_include_late_body_and_exact_boundary() {
    for (feature, elapsed, timeout) in [
        (Feature::HostAssignment, 2500, true),
        (Feature::IssueDuplicate, 2500, false),
        (Feature::RoutinePrecheck, 2000, true),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1beta/decisions", listener.local_addr().unwrap());
        let release = Arc::new(Notify::new());
        let released = release.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let body = fixture();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            ready_tx.send(()).unwrap();
            released.notified().await;
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let gate = gate(endpoint, Arc::new(|| Some(DecisionGateConfig::default())));
        let mut req = request();
        req.scope.feature = feature;
        let decision = tokio::spawn(async move { gate.decide(&req).await });
        await_within(ready_rx, WAIT_CAP).await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_millis(elapsed)).await;
        tokio::time::resume();
        release.notify_one();
        let attempt = await_within(decision, WAIT_CAP).await.unwrap();
        if timeout {
            assert_eq!(
                attempt.result,
                DecisionResult::NoAnswer(NoAnswerReason::Timeout)
            );
        } else {
            assert!(matches!(attempt.result, DecisionResult::Answer { .. }));
        }
        await_within(server, WAIT_CAP).await.unwrap();
    }
}

#[tokio::test]
async fn live_off_discards_an_inflight_answer_and_policy_none_sends_nothing() {
    let config = Arc::new(std::sync::RwLock::new(DecisionGateConfig::default()));
    let source_config = config.clone();
    let source: PolicySource = Arc::new(move || {
        Some(
            source_config
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        )
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1beta/decisions", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = oneshot::channel();
    let (reply_tx, reply_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
        ready_tx.send(()).unwrap();
        await_within(reply_rx, WAIT_CAP).await.unwrap();
        let body = fixture();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let active_gate = gate(endpoint, source);
    let decision = tokio::spawn(async move { active_gate.decide(&request()).await });
    await_within(ready_rx, WAIT_CAP).await.unwrap();
    config.write().unwrap_or_else(|e| e.into_inner()).enabled = false;
    reply_tx.send(()).unwrap();
    assert_eq!(
        await_within(decision, WAIT_CAP).await.unwrap().result,
        DecisionResult::Off(OffReason::GlobalDisabled)
    );
    await_within(server, WAIT_CAP).await.unwrap();
    let gate = gate("http://127.0.0.1:1".into(), Arc::new(|| None));
    assert_eq!(
        gate.decide(&request()).await.result,
        DecisionResult::Off(OffReason::PolicyUnavailable)
    );
}

#[tokio::test]
async fn captured_memory_key_is_redacted_and_install_logs_once() {
    let key = SecretString::new("fixture-only-memory-credential".into()).unwrap();
    assert!(!format!("{key:?}").contains("fixture-only-memory-credential"));
    assert!(key.header().unwrap().is_sensitive());
    assert!(!format!("{:?}", key.header()).contains("fixture-only-memory-credential"));
    let mut command = tokio::process::Command::new("/nonexistent/do-not-spawn-agent-op");
    let (key, outcome) = key::load(
        Some(key),
        &mut command,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert_eq!(outcome, KeyLookupOutcome::On);
    let gate = Arc::new(DecisionGate::new(Arc::new(|| None), key));
    assert!(!format!("{gate:?}").contains("fixture-only-memory-credential"));
    let logs = Logs::default();
    let _capture = tracing::subscriber::set_default(capture(&logs));
    assert!(installed().is_none());
    assert!(install(gate.clone(), outcome));
    assert!(!install(gate, outcome));
    assert!(installed().is_some());
    let text = String::from_utf8(logs.0.lock().unwrap_or_else(|e| e.into_inner()).clone()).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains("WARN"));
    assert!(!text.contains("fixture-only-memory-credential"));
    for (outcome, label) in [
        (KeyLookupOutcome::NoKey, "Off(no_key)"),
        (KeyLookupOutcome::LookupTimeout, "Off(lookup_timeout)"),
        (KeyLookupOutcome::AuthFailed, "Off(auth_failed)"),
        (KeyLookupOutcome::Cancelled, "Off(cancelled)"),
    ] {
        assert_eq!(outcome.boot_label(), label);
    }
}

#[test]
fn request_contract_checks_cardinality_keys_and_all_three_templates() {
    let mut req = request();
    let body = request::body(&req).unwrap();
    assert_eq!(body["input"][0]["text"], req.input_text);
    assert_eq!(
        body["questions"]["q"]["criteria"]
            .as_object()
            .unwrap()
            .len(),
        3
    );
    for size in [0, 1, 256] {
        req.question = Question::Choice {
            instructions: "Pick".into(),
            options: (0..size)
                .map(|i| (i.to_string(), "Choice".into()))
                .collect(),
        };
        assert_eq!(request::body(&req), Err(NoAnswerReason::InvalidRequest));
        req.question = Question::Score {
            instructions: "Rank".into(),
            levels: (0..size).map(|_| "Level".into()).collect(),
        };
        assert_eq!(request::body(&req), Err(NoAnswerReason::InvalidRequest));
    }
    req.question = Question::Choice {
        instructions: "Pick".into(),
        options: vec![("same".into(), "A".into()), ("same".into(), "B".into())],
    };
    assert_eq!(request::body(&req), Err(NoAnswerReason::InvalidRequest));
    req.question = Question::Noul {
        instructions: "Work?".into(),
        true_description: Some("Has work".into()),
        false_description: Some("No work".into()),
    };
    assert_eq!(
        request::body(&req).unwrap()["questions"]["q"]["criteria"]["false_description"],
        "No work"
    );
    req.question = Question::Score {
        instructions: "Rank".into(),
        levels: vec!["Low".into(), "High".into()],
    };
    assert_eq!(
        request::body(&req).unwrap()["questions"]["q"]["criteria"],
        serde_json::json!(["Low", "High"])
    );
    req.input_text = " ".into();
    assert_eq!(request::body(&req), Err(NoAnswerReason::InvalidRequest));
    req = request();
    req.question = Question::Noul {
        instructions: " ".into(),
        true_description: None,
        false_description: None,
    };
    assert_eq!(request::body(&req), Err(NoAnswerReason::InvalidRequest));
}

#[tokio::test]
async fn client_unavailable_transport_and_invalid_header_are_safe_classes() {
    let mut unavailable = gate(
        "http://127.0.0.1:1".into(),
        Arc::new(|| Some(DecisionGateConfig::default())),
    );
    unavailable.client = None;
    assert_eq!(
        unavailable.decide(&request()).await.result,
        DecisionResult::NoAnswer(NoAnswerReason::ClientUnavailable)
    );
    let mut invalid = gate(
        "http://127.0.0.1:1".into(),
        Arc::new(|| Some(DecisionGateConfig::default())),
    );
    invalid.key = SecretString::new("fixture-secret\ninvalid-header".into());
    assert_eq!(
        invalid.decide(&request()).await.result,
        DecisionResult::NoAnswer(NoAnswerReason::ClientUnavailable)
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1beta/decisions", listener.local_addr().unwrap());
    // Closing without a response avoids platform-dependent connection-refusal delays.
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request(&mut stream).await;
    });
    assert_eq!(
        gate(endpoint, Arc::new(|| Some(DecisionGateConfig::default())))
            .decide(&request())
            .await
            .result,
        DecisionResult::NoAnswer(NoAnswerReason::Transport)
    );
    await_within(server, WAIT_CAP).await.unwrap();
}

#[tokio::test]
async fn shared_handle_allows_simultaneous_calls() {
    let (endpoint, calls, server) = server(200, fixture()).await;
    let gate = Arc::new(gate(
        endpoint,
        Arc::new(|| Some(DecisionGateConfig::default())),
    ));
    let first = gate.clone();
    let first = tokio::spawn(async move { first.decide(&request()).await });
    let second = tokio::spawn(async move { gate.decide(&request()).await });
    for task in [first, second] {
        assert!(matches!(
            await_within(task, WAIT_CAP).await.unwrap().result,
            DecisionResult::Answer { .. }
        ));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    stop(server).await;
}

#[tokio::test]
async fn disabled_scopes_and_invalid_requests_never_reach_http() {
    let (endpoint, calls, server) = server(200, fixture()).await;
    let mut config = DecisionGateConfig::default();
    config.features.host_assignment.enabled = false;
    let config = Arc::new(config);
    let gate = gate(endpoint, Arc::new(move || Some((*config).clone())));
    assert_eq!(
        gate.availability(&request().scope),
        Err(OffReason::FeatureDisabled)
    );
    assert_eq!(
        gate.decide(&request()).await.result,
        DecisionResult::Off(OffReason::FeatureDisabled)
    );
    let mut unknown = request();
    unknown.scope.feature = Feature::IssueDuplicate;
    unknown.scope.repository = Some("unknown/repo".into());
    assert_eq!(
        gate.decide(&unknown).await.result,
        DecisionResult::Off(OffReason::RepositoryDisabled)
    );
    let mut invalid = request();
    invalid.scope.feature = Feature::IssueDuplicate;
    invalid.input_text.clear();
    assert_eq!(
        gate.decide(&invalid).await.result,
        DecisionResult::NoAnswer(NoAnswerReason::InvalidRequest)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    stop(server).await;
}
