use super::live::{LiveAttempt, Seen};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::replay_disposition::write::{
    self, BeginFrom, CanonicalInput, EffectProjection, ExactAttempt,
};
use crate::services::provider::CancelToken;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

async fn start(pool: &PgPool, key: &str) -> ExactAttempt {
    let request = CanonicalInput {
        request_key: key.into(),
        provider: "claude".into(),
        channel: "chan".into(),
        sources: vec![format!("{key}-source")],
        original_text: "original".into(),
        owner_id: "user".into(),
        agent_id: "agent".into(),
        attachments: serde_json::json!([]),
        reply_context: None,
        provenance: "new_input",
    };
    let receipt = write::register_or_reuse(pool, &request, "node-a")
        .await
        .unwrap();
    let nonce = receipt.episode_nonce.unwrap();
    let binding = serde_json::json!({});
    let effect = EffectProjection {
        effect_target: "target",
        input_hash: "input",
        binding: &binding,
    };
    let from = BeginFrom::Registered { nonce: &nonce };
    let ack = write::begin(pool, receipt.id, from, &effect, "incarnation-a")
        .await
        .unwrap();
    ack.into_parts().0
}

const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"sid"}"#;
const TEXT: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#;
const TOOL: &str = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#;
const DONE: &str = r#"{"type":"result","subtype":"success","result":"hi","session_id":"sid"}"#;
const FAILED: &str = r#"{"type":"result","is_error":true,"result":"boom","session_id":"sid"}"#;
const UNKNOWN: &str = r#"{"type":"rate_limit_event"}"#;

/// One fake Claude CLI run: its stdout records, stderr text and exit code.
struct Case {
    name: &'static str,
    stdout: &'static [&'static str],
    stderr: &'static str,
    exit: i32,
    receiver_dropped: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "done",
        stdout: &[INIT, TEXT, DONE],
        stderr: "note",
        exit: 0,
        receiver_dropped: false,
    },
    Case {
        name: "error-after-done",
        stdout: &[INIT, TEXT, DONE, FAILED],
        stderr: "",
        exit: 1,
        receiver_dropped: false,
    },
    Case {
        name: "quiet-failure",
        stdout: &[],
        stderr: "No conversation found with session ID: sid",
        exit: 1,
        receiver_dropped: false,
    },
    Case {
        name: "tool",
        stdout: &[INIT, TOOL, DONE],
        stderr: "",
        exit: 0,
        receiver_dropped: false,
    },
    Case {
        name: "unclassified",
        stdout: &[INIT, UNKNOWN],
        stderr: "",
        exit: 1,
        receiver_dropped: false,
    },
    Case {
        name: "malformed",
        stdout: &[INIT, "not json"],
        stderr: "",
        exit: 1,
        receiver_dropped: false,
    },
    Case {
        name: "receiver-dropped",
        stdout: &[INIT, TEXT, DONE],
        stderr: "",
        exit: 0,
        receiver_dropped: true,
    },
];

/// Runs the real direct adapter against a fake CLI per case; returns the messages it sent.
fn run_adapter(
    host: &crate::config::PinnedRuntimeHost,
    tokens: &[Arc<CancelToken>],
) -> Vec<Vec<String>> {
    use std::os::unix::fs::PermissionsExt;
    let cli = host.root.path().join("claude");
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _cli_env = set("AGENTDESK_CLAUDE_PATH", &cli);
    let mut sent = Vec::new();
    for (case, token) in CASES.iter().zip(tokens) {
        let quoted: Vec<String> = case.stdout.iter().map(|line| format!("'{line}'")).collect();
        let script = format!(
            "#!/bin/sh\ncat >/dev/null\n{}\nprintf '%s' '{}' >&2\nexit {}\n",
            if quoted.is_empty() {
                String::new()
            } else {
                format!("printf '%s\\n' {}", quoted.join(" "))
            },
            case.stderr,
            case.exit,
        );
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        // A dropped receiver makes every send fail.
        let rx = (!case.receiver_dropped).then_some(rx);
        let workdir = host.root.path().display().to_string();
        let _ = crate::services::claude::execute_command_streaming(
            "prompt",
            None,
            &workdir,
            tx,
            None,
            None,
            Some(Arc::clone(token)),
            None,
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
        );
        sent.push(rx.map_or_else(Vec::new, |rx| {
            rx.try_iter()
                .map(|message| format!("{message:?}"))
                .collect()
        }));
    }
    sent
}

async fn live_attempts(pool: &PgPool, prefix: &str) -> Vec<(Arc<LiveAttempt>, Arc<CancelToken>)> {
    let mut attempts = Vec::new();
    for case in CASES {
        let live = Arc::new(LiveAttempt::new(
            start(pool, &format!("{prefix}-{}", case.name)).await,
        ));
        let token = Arc::new(CancelToken::new());
        assert!(token.attach_replay_attempt(Arc::clone(&live)));
        attempts.push((live, token));
    }
    attempts
}

// The adapter records past its Done to stdout end, wait and stderr: one provider terminal
// classifies, a later Error does not, and only a quiet complete run is no-effect.
#[tokio::test(flavor = "current_thread")]
async fn direct_adapter_completes_the_observation_past_its_done_pg() {
    let host = crate::config::pin_runtime_host_for_test();
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    let attempts = live_attempts(&pool, "raw").await;
    let tokens: Vec<_> = attempts
        .iter()
        .map(|(_, token)| Arc::clone(token))
        .collect();
    run_adapter(&host, &tokens);
    let mut judged = Vec::new();
    for (case, (live, _)) in CASES.iter().zip(&attempts) {
        let (_, observation) = tokio::time::timeout(
            Duration::from_secs(5),
            live.take_for_decision(Duration::from_secs(30)),
        )
        .await
        .unwrap_or_else(|_| panic!("{}: the reader never finished", case.name))
        .unwrap();
        let terminal = observation
            .terminal_evidence()
            .map(|evidence| evidence.kind());
        let no_effect = observation.no_effect_evidence().is_some();
        judged.push((case.name, terminal, no_effect));
    }
    use super::live::TerminalKind::Done;
    assert_eq!(
        judged,
        [
            ("done", Some(Done), false),
            ("error-after-done", None, false),
            ("quiet-failure", None, true),
            ("tool", Some(Done), false),
            ("unclassified", None, false),
            ("malformed", None, false),
            ("receiver-dropped", None, false),
        ]
    );
    pool.close().await;
    fixture.drop().await;
}

// Without a live attempt on its token the adapter sends exactly the messages it sends with one,
// and nothing attaches an attempt for it.
#[tokio::test(flavor = "current_thread")]
async fn direct_adapter_without_a_live_attempt_is_unchanged_pg() {
    let host = crate::config::pin_runtime_host_for_test();
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    let attempts = live_attempts(&pool, "dormant").await;
    let live_tokens: Vec<_> = attempts
        .iter()
        .map(|(_, token)| Arc::clone(token))
        .collect();
    let plain: Vec<_> = CASES.iter().map(|_| Arc::new(CancelToken::new())).collect();
    let with_live = run_adapter(&host, &live_tokens);
    let without = run_adapter(&host, &plain);
    assert_eq!(without, with_live);
    assert!(
        with_live[0]
            .iter()
            .any(|message| message.starts_with("Done"))
    );
    assert!(plain.iter().all(|token| token.replay_attempt().is_none()));
    pool.close().await;
    fixture.drop().await;
}

// A decision registers for the reader's notice before checking the record, so the last fact
// arriving between that check and the wait wakes it at once.
#[tokio::test(flavor = "current_thread")]
async fn decision_wakes_on_a_fact_recorded_after_its_check_pg() {
    let host = crate::config::pin_runtime_host_for_test();
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    let live = LiveAttempt::new(start(&pool, "barrier").await);
    live.record(Seen::StdoutEof);
    live.record(Seen::StderrDrained);
    live.after_check
        .lock()
        .unwrap()
        .push(Seen::Exited { waited: true });
    let decided = tokio::time::timeout(
        Duration::from_secs(5),
        live.take_for_decision(Duration::from_secs(60)),
    )
    .await
    .expect("the fact recorded after the check must wake the decision");
    let (_, observation) = decided.unwrap();
    assert!(observation.no_effect_evidence().is_some());
    pool.close().await;
    fixture.drop().await;
}
