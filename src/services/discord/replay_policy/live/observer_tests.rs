use super::*;
use crate::config::TestEnvVarGuard as Guard;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::replay_disposition::write::{self, BeginFrom, CanonicalInput, EffectProjection};
use crate::services::provider::{CancelToken, ProviderKind};
use serde_json::{Value, json};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

struct Host {
    _vars: Vec<Guard>,
    _root: Guard,
    dir: tempfile::TempDir,
}

fn host() -> Host {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = crate::config::set_agentdesk_root_for_test(dir.path());
    let cli = dir.path().join("fixture-cli");
    std::fs::write(
        &cli,
        include_str!("../../../../../tests/fixtures/i7330/u4_cli.py"),
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut vars: Vec<_> = [
        "AGENTDESK_CLAUDE_PATH",
        "AGENTDESK_CODEX_PATH",
        "AGENTDESK_GEMINI_PATH",
        "AGENTDESK_QWEN_PATH",
    ]
    .map(|key| Guard::set_path_after_shared_test_env_lock(key, &cli))
    .into_iter()
    .collect();
    vars.push(Guard::set_path_after_shared_test_env_lock(
        "CLAUDE_CONFIG_DIR",
        dir.path(),
    ));
    Host {
        _vars: vars,
        _root: root,
        dir,
    }
}

struct Case {
    _env: Guard,
    dir: tempfile::TempDir,
}

impl Case {
    fn new(host: &Host, cfg: Value) -> Self {
        let dir = tempfile::tempdir_in(host.dir.path()).unwrap();
        std::fs::write(dir.path().join("case.json"), cfg.to_string()).unwrap();
        let env = Guard::set_path_after_shared_test_env_lock("ADK_U4_CASE_DIR", dir.path());
        Self { _env: env, dir }
    }

    fn release(&self, name: &str) {
        std::fs::write(self.dir.path().join(name), "1").unwrap();
    }

    fn wait(&self, name: &str) {
        until(|| self.dir.path().join(name).exists(), name);
    }

    fn starts(&self) -> Vec<u32> {
        std::fs::read_to_string(self.dir.path().join("starts"))
            .unwrap()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        for name in ["record_go", "stdout_go", "stderr_go", "exit_go", "warm_end"] {
            let _ = std::fs::write(self.dir.path().join(name), "1");
        }
        if self.dir.path().join("ready").exists() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn until(mut predicate: impl FnMut() -> bool, label: &str) {
    let end = Instant::now() + Duration::from_secs(20);
    while !predicate() {
        assert!(Instant::now() < end, "U4 barrier timed out: {label}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn records(provider: &str, mode: &str) -> Vec<String> {
    let mut records = Vec::new();
    let record = match (provider, mode) {
        (_, "quiet") => None,
        ("codex", "tool") => Some(
            json!({"type":"item.started","item":{"type":"command_execution","id":"u4-tool","command":"echo measured"}}),
        ),
        ("codex", _) => {
            Some(json!({"type":"item.completed","item":{"type":"agent_message","text":"captured"}}))
        }
        ("gemini", "tool") => Some(
            json!({"type":"tool_use","tool_name":"run_shell_command","tool_id":"u4-tool","parameters":{"command":"echo measured"}}),
        ),
        ("gemini", _) => Some(json!({"type":"message","role":"assistant","content":"captured"})),
        (_, "tool") => Some(
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"u4-tool","name":"Bash","input":{}}]}}),
        ),
        _ => Some(
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"captured"}]}}),
        ),
    };
    if let Some(record) = record {
        records.push(record.to_string());
    }
    if mode == "success" {
        records.push(match provider {
            "codex" => json!({"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}),
            _ => json!({"type":"result","subtype":"success","result":"captured","is_error":false}),
        }.to_string());
    }
    records
}

async fn live(pool: &sqlx::PgPool, provider: &str, label: &str) -> Arc<LiveAttempt> {
    let key = format!("p2-observer-{provider}-{label}");
    let input = CanonicalInput {
        request_key: key.clone(),
        provider: provider.into(),
        channel: "7330".into(),
        sources: vec![key],
        original_text: "measured".into(),
        owner_id: "owner".into(),
        agent_id: "agent".into(),
        attachments: json!([]),
        reply_context: None,
        provenance: "new_input",
    };
    let receipt = write::register_or_reuse(pool, &input, "u4-node")
        .await
        .unwrap();
    let binding = json!({});
    let ack = write::begin(
        pool,
        receipt.id,
        BeginFrom::Registered {
            nonce: receipt.episode_nonce.as_deref().unwrap(),
        },
        &EffectProjection {
            effect_target: "provider_start",
            input_hash: "u4",
            binding: &binding,
        },
        "u4-incarnation",
    )
    .await
    .unwrap();
    Arc::new(LiveAttempt::new(ack.into_parts().0))
}

fn token(live: &Arc<LiveAttempt>) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    assert!(token.attach_replay_attempt(Arc::clone(live)));
    token
}

fn invoke(
    provider: &str,
    cwd: &str,
    sender: mpsc::Sender<StreamMessage>,
    token: Arc<CancelToken>,
) -> Result<(), String> {
    match provider {
        "codex" => crate::services::codex::execute_command_streaming(
            "probe",
            Some("u4-native"),
            cwd,
            sender,
            None,
            None,
            Some(token),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
        ),
        "claude" => crate::services::claude::execute_command_streaming(
            "probe",
            Some("u4-native"),
            cwd,
            sender,
            None,
            None,
            Some(token),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
        ),
        _ => unreachable!(),
    }
}

fn counts(messages: &[StreamMessage]) -> Value {
    json!({"text":messages.iter().filter(|m| matches!(m, StreamMessage::Text { .. })).count(), "tool":messages.iter().filter(|m| matches!(m, StreamMessage::ToolUse { .. } | StreamMessage::ToolResult { .. })).count(), "done":messages.iter().filter(|m| matches!(m, StreamMessage::Done { .. })).count(), "error":messages.iter().filter(|m| matches!(m, StreamMessage::Error { .. })).count()})
}

fn direct(host: &Host, provider: &'static str, mode: &str, order: &[&str], live: Arc<LiveAttempt>) {
    let case = Case::new(
        host,
        json!({"records":records(provider, mode),"barriers":true,"stderr":"No conversation found with session ID: u4-native","exit":1}),
    );
    let (tx, rx) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let cwd = host.dir.path().to_str().unwrap().to_string();
    let tok = token(&live);
    let reader = std::thread::spawn(move || {
        done.send(invoke(provider, &cwd, tx, tok)).unwrap();
    });
    case.wait("ready");
    assert_eq!(case.starts().len(), 1, "actual CLI must have started once");
    let mut phases = Vec::new();
    for (index, gate) in order.iter().enumerate() {
        case.release(gate);
        match *gate {
            "stdout_go" => case.wait("stdout_closed"),
            "stderr_go" => case.wait("stderr_closed"),
            "exit_go" => {
                case.wait("exiting");
                let pid = case.starts()[0];
                until(
                    || {
                        std::fs::read_to_string(format!("/proc/{pid}/stat"))
                            .map_or(true, |s| s.contains(") Z "))
                    },
                    "owned CLI exit",
                );
            }
            _ => unreachable!(),
        }
        std::thread::sleep(Duration::from_millis(40));
        let observation = lock(&live.observation).clone();
        if index + 1 < order.len() {
            assert!(
                !observation.complete(),
                "missing physical barrier cannot be complete"
            );
            assert!(observation.no_effect_evidence().is_none());
            assert!(matches!(
                finished.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
        }
        phases.push(json!({"gate":gate,"facts":format!("{observation:?}"),"no_effect":observation.no_effect_evidence().is_some()}));
    }
    finished
        .recv_timeout(Duration::from_secs(20))
        .unwrap()
        .unwrap();
    reader.join().unwrap();
    let messages: Vec<_> = rx.try_iter().collect();
    let observed = lock(&live.observation).clone();
    let c = counts(&messages);
    if mode == "text" {
        assert!(c["text"].as_u64().unwrap() > 0);
    }
    if mode == "tool" {
        assert!(c["tool"].as_u64().unwrap() > 0);
    }
    println!(
        "P2_OBSERVER_RESULT {}",
        json!({"runtime":provider,"mode":mode,"order":order,"starts":case.starts().len(),"messages":c,"facts":format!("{observed:?}"),"no_effect":observed.no_effect_evidence().is_some(),"phases":phases})
    );
    assert_eq!(
        observed.no_effect_evidence().is_some(),
        mode == "quiet",
        "quiet completed direct runtime must provide evidence"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn p2_codex_quiet_stale_observer_pg() {
    let host = host();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let live = live(&pool, "codex", "quiet").await;
    direct(
        &host,
        "codex",
        "quiet",
        &["stdout_go", "exit_go", "stderr_go"],
        live,
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn p2_ordinary_completion_defers_a_stale_error_pg() {
    let host = host();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let live = live(&pool, "claude", "stale-error").await;
    let tok = token(&live);
    let state = inflight(&live);
    let id = state.replay_receipt_id.unwrap();
    {
        let _case = Case::new(
            &host,
            json!({"records":[json!({"type":"result","is_error":true,"result":"No conversation found with session ID: u4-native"}).to_string()],"stderr":"No conversation found with session ID: u4-native","exit":1}),
        );
        let (tx, rx) = mpsc::channel();
        invoke(
            "claude",
            host.dir.path().to_str().unwrap(),
            tx,
            Arc::clone(&tok),
        )
        .unwrap();
        assert!(
            counts(&rx.try_iter().collect::<Vec<_>>())["error"]
                .as_u64()
                .unwrap()
                > 0
        );
    }
    assert!(
        !super::super::ordinary_outcome(Some(&pool), &tok, &state).await,
        "ordinary completion cannot classify a pending stale provider Error"
    );
    let stored: String =
        sqlx::query_scalar("SELECT replay_disposition FROM intake_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, "started_unclassified");
    assert!(!super::super::error_is_ordinary(Some(&pool), &tok, &state, true).await);
    assert!(matches!(
        super::super::stale_resume(Some(&pool), &tok, &state).await,
        super::super::StaleResume::Exact(_)
    ));
    assert!(matches!(
        super::super::stale_resume(Some(&pool), &tok, &state).await,
        super::super::StaleResume::Hold
    ));
    pool.close().await;
    db.drop().await;
}

#[test]
fn p2_only_owned_direct_runtimes_can_activate() {
    use crate::services::agent_protocol::RuntimeHandoffKind as R;
    use crate::services::provider_hosting::ProviderSessionDriver as D;
    for provider in [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Gemini,
        ProviderKind::Qwen,
    ] {
        for driver in [D::LegacyPrompt, D::TuiHosting] {
            for runtime in [
                None,
                Some(R::ProcessBackend),
                Some(R::LegacyTmuxWrapper),
                Some(R::ClaudeTui),
                Some(R::CodexTui),
            ] {
                let expected = matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
                    && driver == D::LegacyPrompt
                    && runtime.is_none();
                assert_eq!(
                    seal::can_activate(provider.clone(), driver, runtime),
                    expected,
                    "{provider:?}/{driver:?}/{runtime:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn p2_physical_facts_need_one_owned_seal_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    for label in ["finish", "drop", "duplicate", "timeout"] {
        let live = live(&pool, "codex", label).await;
        let tok = token(&live);
        let tap = RawTap::for_codex(Some(&tok));
        for fact in [
            Seen::StdoutEof,
            Seen::Exited { waited: true },
            Seen::StderrDrained,
        ] {
            tap.see(fact);
        }
        assert!(
            lock(&live.observation).no_effect_evidence().is_none(),
            "physical facts cannot replace owner close"
        );
        match label {
            "finish" => {
                tap.finish(true, Seen::StderrDrained);
                assert!(lock(&live.observation).no_effect_evidence().is_some());
            }
            "duplicate" => {
                let duplicate = RawTap::for_codex(Some(&tok));
                assert!(!duplicate.active());
                tap.finish(true, Seen::StderrDrained);
                assert!(lock(&live.observation).no_effect_evidence().is_none());
            }
            "timeout" => {
                let (_, observed) = live.take_for_decision(Duration::ZERO).await.unwrap();
                assert!(observed.no_effect_evidence().is_none());
                tap.finish(true, Seen::StderrDrained);
                assert!(live.take_for_decision(Duration::ZERO).await.is_none());
            }
            _ => {
                drop(tap);
                assert!(lock(&live.observation).no_effect_evidence().is_none());
            }
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn p2_actual_output_and_tool_before_stale_never_get_retry_pg() {
    let host = host();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    for provider in ["claude", "codex"] {
        for mode in ["text", "tool"] {
            let live = live(&pool, provider, mode).await;
            let tok = token(&live);
            direct(
                &host,
                provider,
                mode,
                if mode == "text" {
                    &["exit_go", "stderr_go", "stdout_go"]
                } else {
                    &["stderr_go", "stdout_go", "exit_go"]
                },
                Arc::clone(&live),
            );
            let inflight = inflight(&live);
            assert!(matches!(
                super::super::stale_resume(Some(&pool), &tok, &inflight).await,
                super::super::StaleResume::Hold
            ));
            let state: String =
                sqlx::query_scalar("SELECT replay_disposition FROM intake_outbox WHERE id=$1")
                    .bind(inflight.replay_receipt_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(state, "withheld");
        }
    }
    pool.close().await;
    db.drop().await;
}

fn inflight(live: &LiveAttempt) -> crate::services::discord::inflight::InflightTurnState {
    let mut state = crate::services::discord::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        7330,
        None,
        1,
        101,
        102,
        "probe".into(),
        None,
        None,
        None,
        None,
        0,
    );
    state.replay_receipt_id = lock(&live.attempt).as_ref().map(ExactAttempt::receipt_id);
    state
}

#[tokio::test(flavor = "current_thread")]
async fn p2_file_reader_cannot_mint_no_effect_after_wrapper_exit_pg() {
    let host = host();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let observed = live(&pool, "claude", "file").await;
    file_backend(&host, "tool", Arc::clone(&observed));
    let facts = lock(&observed.observation).clone();
    assert!(
        facts.active(),
        "actual tool must be observed before the normalized callback"
    );
    assert!(facts.seen.contains(&Seen::Broken));
    assert!(facts.no_effect_evidence().is_none());
    pool.close().await;
    db.drop().await;
}

fn file_backend(host: &Host, mode: &str, live: Arc<LiveAttempt>) {
    use crate::services::provider::SessionProbe;
    use crate::services::session_backend::{ProcessBackend, SessionBackend, SessionConfig};
    let case = Case::new(
        host,
        json!({"records":records("claude",mode),"file":true,"barriers":true,"exit":1}),
    );
    let output = case.dir.path().join("stream.jsonl");
    let prompt = case.dir.path().join("prompt");
    std::fs::write(&prompt, "probe").unwrap();
    let backend = ProcessBackend::new();
    let handle = Arc::new(
        backend
            .create_session(&SessionConfig {
                session_name: "u4-process-fixture".into(),
                working_dir: host.dir.path().display().to_string(),
                agentdesk_exe: host.dir.path().join("fixture-cli").display().to_string(),
                output_path: output.display().to_string(),
                prompt_path: prompt.display().to_string(),
                wrapper_subcommand: "fixture".into(),
                wrapper_args: vec![],
                env_vars: vec![],
                unset_env: vec![],
            })
            .unwrap(),
    );
    case.wait("ready");
    let stderr_captured = match handle.as_ref() {
        crate::services::session_backend::SessionHandle::Process { child, .. } => {
            child.lock().unwrap().as_ref().unwrap().stderr.is_some()
        }
        _ => unreachable!(),
    };
    let (tx, rx) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let polled = Arc::clone(&handle);
    let path = output.display().to_string();
    let tok = token(&live);
    let reader = std::thread::spawn(move || {
        let result = crate::services::session_backend::read_output_file_until_result_with_harvest(
            &path,
            0,
            tx,
            Some(tok),
            SessionProbe::new(move || ProcessBackend::new().is_alive(&polled), || false),
        );
        done.send(result).unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        matches!(finished.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "temporary file EOF must not finish a live writer"
    );
    case.release("exit_go");
    case.wait("exiting");
    let result = finished
        .recv_timeout(Duration::from_secs(20))
        .unwrap()
        .unwrap();
    assert!(matches!(
        result.0,
        crate::services::provider::ReadOutputResult::SessionDied { .. }
    ));
    let before = std::fs::metadata(&output).unwrap().len();
    case.release("stdout_go");
    case.wait("stdout_closed");
    let after = std::fs::metadata(&output).unwrap().len();
    assert!(
        after > before,
        "a descendant can append after the wrapper was reaped"
    );
    case.release("stderr_go");
    case.wait("stderr_closed");
    reader.join().unwrap();
    let messages: Vec<_> = rx.try_iter().collect();
    assert!(!stderr_captured);
    println!(
        "U4_RESULT {}",
        json!({"runtime":"ProcessBackend","mode":mode,"starts":case.starts().len(),"stderr_captured":stderr_captured,"read_result":format!("{:?}",result.0),"source_identity":format!("{:?}",result.1.source_file),"late_growth":after-before,"messages":counts(&messages),"facts":format!("{:?}",lock(&live.observation))})
    );
}

#[tokio::test(flavor = "current_thread")]
async fn p2_quiet_retry_is_exact_once_and_success_is_one_response_pg() {
    let host = host();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    for success in [false, true] {
        let first = live(
            &pool,
            "codex",
            if success {
                "retry-success"
            } else {
                "retry-exhausted"
            },
        )
        .await;
        let tok = token(&first);
        let state = inflight(&first);
        let id = state.replay_receipt_id.unwrap();
        let nonce = lock(&first.attempt).as_ref().unwrap().nonce().to_string();
        let session = format!("p2-retry-{success}");
        sqlx::query("INSERT INTO sessions (session_key,channel_id,provider,status,current_replay_receipt_id,replay_episode_nonce,claude_session_id) VALUES ($1,'7330','codex','turn_active',$2,$3,'stale')").bind(&session).bind(id).bind(&nonce).execute(&pool).await.unwrap();
        direct(
            &host,
            "codex",
            "quiet",
            &["stdout_go", "exit_go", "stderr_go"],
            Arc::clone(&first),
        );
        let super::super::StaleResume::Exact(permit) =
            super::super::stale_resume(Some(&pool), &tok, &state).await
        else {
            panic!("one exact startup retry required");
        };
        assert!(matches!(
            super::super::stale_resume(Some(&pool), &tok, &state).await,
            super::super::StaleResume::Hold
        ));
        let binding = json!({});
        let (attempt, _, _) = write::begin(
            &pool,
            id,
            BeginFrom::NoEffect {
                previous: permit.previous_nonce(),
                next: permit.next_nonce(),
                session_key: &session,
            },
            &EffectProjection {
                effect_target: "provider_start",
                input_hash: "u4",
                binding: &binding,
            },
            "retry-incarnation",
        )
        .await
        .unwrap()
        .into_parts();
        assert!(attempt.retry_of().is_some());
        let next = Arc::new(LiveAttempt::new(attempt));
        let next_token = token(&next);
        let next_state = inflight(&next);
        if success {
            let case = Case::new(
                &host,
                json!({"records":records("codex","success"),"stderr":"","exit":0}),
            );
            let (tx, rx) = mpsc::channel();
            invoke("codex", host.dir.path().to_str().unwrap(), tx, token(&next)).unwrap();
            assert_eq!(case.starts().len(), 1);
            assert_eq!(counts(&rx.try_iter().collect::<Vec<_>>())["done"], 1);
            assert!(super::super::ordinary_outcome(Some(&pool), &next_token, &next_state).await);
        } else {
            direct(
                &host,
                "codex",
                "quiet",
                &["stderr_go", "exit_go", "stdout_go"],
                Arc::clone(&next),
            );
            assert!(matches!(
                super::super::stale_resume(Some(&pool), &next_token, &next_state).await,
                super::super::StaleResume::Hold
            ));
            let reason: String =
                sqlx::query_scalar("SELECT replay_hold_reason FROM intake_outbox WHERE id=$1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(reason, "startup retry budget exhausted");
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn p2_first_output_on_either_side_of_terminal_wait_is_held_pg() {
    let host = host();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    for before in [true, false] {
        let observed = live(
            &pool,
            "codex",
            if before {
                "before-output"
            } else {
                "after-output"
            },
        )
        .await;
        let tok = token(&observed);
        let state = inflight(&observed);
        let case = Case::new(
            &host,
            json!({"records":records("codex","text"),"delay_records":true,"barriers":true,"stderr":"No conversation found with session ID: u4-native","exit":1}),
        );
        let (tx, rx) = mpsc::channel();
        let cwd = host.dir.path().display().to_string();
        let reader_token = Arc::clone(&tok);
        let reader = std::thread::spawn(move || invoke("codex", &cwd, tx, reader_token));
        case.wait("records_waiting");
        if !before {
            case.release("record_go");
            case.wait("ready");
            until(
                || lock(&observed.observation).active(),
                "raw output before terminal wait",
            );
        }
        let normal = super::super::ordinary_outcome(Some(&pool), &tok, &state);
        tokio::pin!(normal);
        assert!(
            futures::poll!(&mut normal).is_pending(),
            "unsealed reader must keep terminal pending"
        );
        assert!(lock(&observed.observation).no_effect_evidence().is_none());
        if before {
            case.release("record_go");
            case.wait("ready");
        }
        for gate in ["stdout_go", "exit_go", "stderr_go"] {
            case.release(gate);
        }
        reader.join().unwrap().unwrap();
        assert_eq!(case.starts().len(), 1);
        let counts = counts(&rx.try_iter().collect::<Vec<_>>());
        assert_eq!(counts["text"], 1);
        assert_eq!(counts["error"], 1);
        assert!(
            !normal.await,
            "raw stale wins before ordinary classification"
        );
        assert!(matches!(
            super::super::stale_resume(Some(&pool), &tok, &state).await,
            super::super::StaleResume::Hold
        ));
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn p3_unconsumed_stderr_cannot_stall_terminal_classification_pg() {
    let host = host();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    for (label, record, exit) in [
        (
            "ordinary-error",
            json!({"type":"error","message":"boom"}),
            1,
        ),
        ("success", json!({"type":"turn.completed","usage":{}}), 0),
    ] {
        let live = live(&pool, "codex", label).await;
        let tok = token(&live);
        let state = inflight(&live);
        let _case = Case::new(
            &host,
            json!({"records":[record.to_string()],
            "stderr":"could not find session with id old", "exit":exit}),
        );
        let (tx, rx) = mpsc::channel();
        invoke(
            "codex",
            host.dir.path().to_str().unwrap(),
            tx,
            Arc::clone(&tok),
        )
        .unwrap();
        let messages: Vec<_> = rx.try_iter().collect();
        assert!(!messages.is_empty(), "actual bridge input must exist");
        assert!(
            super::super::error_is_ordinary(Some(&pool), &tok, &state, false).await,
            "stderr absent from the consumed Error cannot defer the decision: {label}"
        );
        let stored: String =
            sqlx::query_scalar("SELECT replay_disposition FROM intake_outbox WHERE id=$1")
                .bind(state.replay_receipt_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, "classified_normal");
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn p3_file_text_is_activity_not_a_stale_error_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let live = live(&pool, "claude", "file-phrase").await;
    let tok = token(&live);
    let tap = RawTap::for_file(Some(&tok));
    tap.file_activity();
    tap.finish(true, Seen::StderrDrained);
    assert!(!live.stale(), "assistant text is not a bridge Error");
    assert!(!super::super::ordinary_outcome(Some(&pool), &tok, &inflight(&live)).await);
    assert_eq!(live.verdict(), Some(Verdict::Held));
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn recovery_codex_stderr_descendant_is_cancelled_and_joined_pg() {
    let host = host();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let live = live(&pool, "codex", "descendant").await;
    let tok = token(&live);
    let case = Case::new(
        &host,
        json!({"records":[],"barriers":true,
        "stderr":"could not find session with id old", "stderr_append":true, "exit":1}),
    );
    let (tx, rx) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let cwd = host.dir.path().to_str().unwrap().to_string();
    let reader_token = Arc::clone(&tok);
    let reader = std::thread::spawn(move || {
        done.send(invoke("codex", &cwd, tx, reader_token)).unwrap();
    });
    case.wait("ready");
    assert_eq!(case.starts().len(), 1);
    let workers = || {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(Result::ok)
            .filter(|task| {
                std::fs::read_to_string(task.path().join("comm"))
                    .is_ok_and(|name| name.trim() == "replay-stderr")
            })
            .count()
    };
    assert_eq!(
        workers(),
        1,
        "fixture must have reached the owned stderr worker"
    );
    let started = Instant::now();
    case.release("stdout_go");
    case.release("exit_go");
    finished
        .recv_timeout(Duration::from_secs(8))
        .unwrap()
        .unwrap();
    reader.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(8));
    assert_eq!(workers(), 0, "timed out pipe reader must be joined");
    case.release("stderr_go");
    case.wait("stderr_closed");
    assert!(
        case.dir.path().join("stderr_reader_closed").exists(),
        "descendant append sees the closed read descriptor"
    );
    assert!(
        !rx.try_iter().collect::<Vec<_>>().is_empty(),
        "bridge Error must be delivered"
    );
    assert!(!super::super::error_is_ordinary(Some(&pool), &tok, &inflight(&live), false).await);
    assert_eq!(live.verdict(), Some(Verdict::Held));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn p4_stale_reader_without_bridge_stale_decision_is_durably_held_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    let live = live(&pool, "claude", "unmatched-stale").await;
    let tok = token(&live);
    let state = inflight(&live);
    live.record(Seen::Broken);
    live.mark_stale();
    assert!(!super::super::error_is_ordinary(Some(&pool), &tok, &state, false).await);
    let stored: String =
        sqlx::query_scalar("SELECT replay_disposition FROM intake_outbox WHERE id=$1")
            .bind(state.replay_receipt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        stored, "withheld",
        "unmatched stale cannot stay unclassified"
    );
    assert_eq!(live.verdict(), Some(Verdict::Held));
    pool.close().await;
    db.drop().await;
}
