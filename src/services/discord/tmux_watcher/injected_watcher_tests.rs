use super::o_seed_install_tests as seed;
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::inflight::o_seed_observation::TerminalBarrier;
use crate::services::discord::replay_policy::injected::test_support as c;
use crate::services::discord::tmux::InstallFixture;
use crate::services::tui_prompt_dedupe::{TuiRuntimeBinding, register_tmux_runtime_binding};

const PROMPT: &str = "C-nonce: exact injected input";
const SESSION: &str = "B-session";

#[path = "injected_bridge_tests.rs"]
mod injected_bridge_tests;

fn child(test: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_env_filter("agentdesk::relay_flight_recorder=info,agentdesk::services::discord::tmux::tmux_watcher=info")
            .with_writer(|| Capture)
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("child log capture");
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let tmux = root.path().join("tmux");
    std::fs::write(
        &tmux,
        FAKE_TMUX.replace("ROOT", &root.path().display().to_string()),
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let exact = format!("{}::{test}", module_path!().split_once("::").unwrap().1);
    let selected: Vec<_> = (100..115)
        .chain(std::iter::once(201))
        .map(|case| (600_820_000 + case, "claude_tui"))
        .collect();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env(CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("PATH", root.path())
        .env("AGENTDESK_STATUS_INTERVAL_SECS", "0")
        .env(
            crate::services::tui_o::cutover::test_override::CHILD_ENV,
            "1",
        )
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            serde_json::to_string(&selected).unwrap(),
        )
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("CHILD exact={exact} args=--exact,--nocapture\n{stdout}\n{stderr}");
    assert!(
        output.status.success() && stdout.contains("1 passed; 0 failed"),
        "actual watcher child failed"
    );
    assert!(
        std::fs::read_to_string(root.path().join("tmux-errors"))
            .unwrap_or_default()
            .is_empty()
    );
    false
}

fn bind(f: &InstallFixture) {
    register_tmux_runtime_binding(
        &f.session,
        TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: f.output.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(SESSION.into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
}

fn record(kind: &str) -> String {
    let event = match kind {
        "enqueue" => {
            serde_json::json!({"type":"queue-operation","operation":"enqueue","content":PROMPT})
        }
        "marker" => {
            serde_json::json!({"type":"system","subtype":"queued_command","content":PROMPT})
        }
        "wrong_hash" => {
            serde_json::json!({"type":"user", "sessionId":SESSION, "message":{"role":"user", "content":"C-nonce: changed full input"}})
        }
        "wrong_nonce" => {
            serde_json::json!({"type":"user","sessionId":SESSION,"message":{"role":"user","content":"other-nonce: exact injected input"}})
        }
        "wrong_session" => {
            serde_json::json!({"type":"user","sessionId":"foreign-session","message":{"role":"user","content":PROMPT}})
        }
        _ => {
            serde_json::json!({"type":"user","sessionId":SESSION,"message":{"role":"user","content":PROMPT}})
        }
    };
    format!("{event}\n")
}

fn terminal_record() -> String {
    format!(
        "{}\n",
        serde_json::json!({"type":"result", "sessionId":SESSION, "is_error":false, "result":"C result"})
    )
}

async fn case(
    case: u64,
    retired: bool,
    mode: &str,
    receipt: bool,
) -> (Vec<String>, Vec<&'static str>) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    std::fs::write(
        crate::config::runtime_root().unwrap().join("pane"),
        "busy\n",
    )
    .unwrap();
    let mut f = InstallFixture::new_with_pool(CLAUDE, 600_820_000 + case, Some(pool.clone())).await;
    use crate::services::discord::health::legacy_supervision::test_support::MockDiscord;
    let discord = MockDiscord::start().await;
    f.http = discord.http.clone();
    bind(&f);
    let _retired = retired.then(|| RetiredForTest::new("claude", f.channel.get()));
    let observed = seed::observe(&f);
    let barrier = TerminalBarrier::arm(&CLAUDE, f.channel.get());
    let submissions =
        crate::services::discord::task_supervisor::n4d_spawn_observation::Scope::new(&f.shared);
    let handle = if retired {
        assert_eq!(
            f.install().await,
            crate::services::discord::OOnlyInstallOutcome::Spawned
        );
        f.handle()
    } else {
        seed::spawn_legacy(&f, 0, None)
    };
    seed::until(&observed, "watcher entered EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let owners = f.shared.tmux_watchers.len();
    let id = if receipt {
        Some(
            c::attach(
                &f.shared,
                c::Input {
                    channel: f.channel.get(),
                    pane: &f.session,
                    path: &f.output,
                    owner_nonce: "B-owner",
                    session: SESSION,
                    text: PROMPT,
                    frontier: 0,
                },
            )
            .await,
        )
    } else {
        None
    };
    if mode == "cas_miss" {
        c::withhold(f.channel.get(), id.unwrap(), &pool).await;
    }
    if mode == "ack_lost" {
        c::lose_ack(f.channel.get());
    }
    if mode == "wrong_attempt" {
        let other = c::attach(
            &f.shared,
            c::Input {
                channel: f.channel.get(),
                pane: &f.session,
                path: &f.output,
                owner_nonce: "B-owner",
                session: SESSION,
                text: "C-nonce other",
                frontier: 0,
            },
        )
        .await;
        c::wrong_attempt(f.channel.get(), id.unwrap(), other);
    }
    let log_start = LOG.lock().unwrap().len();
    let mut bytes = record(mode);
    if mode == "before_input" {
        bytes = format!("{}{}", terminal_record(), bytes);
    }
    if mode == "relay_suppressed" {
        bytes.push_str(&format!("{}\n", serde_json::json!({"type":"system", "subtype":"task_notification",
            "task_id":format!("suppressed-{case}"), "status":"completed", "summary":"C task finished",
            "task_notification_kind":"background"})));
    } else {
        bytes.push_str(&said("C result"));
    }
    if mode != "paste_only" && mode != "before_input" {
        bytes.push_str(&if mode == "direct_denial" {
            stop()
        } else {
            terminal_record()
        });
    }
    seed::append(&f, bytes.as_bytes());
    if mode == "direct_denial" {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
        loop {
            let log = String::from_utf8_lossy(&LOG.lock().unwrap()[log_start..]).into_owned();
            if log.contains("route=\"soft_terminal_no_authority:no_inflight_row\"") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "actual direct fallback denial was not reached: snapshot={:?} log={log}",
                observed.snapshot()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    } else if mode == "paste_only" {
        seed::until(&observed, "nonterminal source parsed", |s| {
            !s.parser_responses.is_empty()
        })
        .await;
    } else {
        tokio::time::timeout(Duration::from_secs(11), barrier.wait())
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "terminal fixture failed: {error} snapshot={:?} log={}",
                    observed.snapshot(),
                    String::from_utf8_lossy(&LOG.lock().unwrap()[log_start..])
                );
            });
        assert!(
            c::trace(f.channel.get()).is_empty(),
            "collector cannot classify before dispatch checkpoint"
        );
        assert_eq!(
            observed.snapshot().event_count("retired_terminal_consumed"),
            0
        );
        if !receipt {
            let snapshot = observed.snapshot();
            assert_eq!(snapshot.event_count("parsed_terminal"), 1);
            assert_eq!(snapshot.event_count("checkpoint"), 1);
            assert!(discord.requests_for(f.channel.get()).is_empty());
            eprintln!(
                "LEGACY_PHASE retired={retired} calls=parse,dispatch_checkpoint HTTP=0 custody_ACK=0 C=0"
            );
        }
        match mode {
            "cancel" => cancel_for_test(&handle.cancel, Ordering::Release),
            "authority" => {
                handle.pause_epoch.fetch_add(1, Ordering::AcqRel);
            }
            "pre_emit_reject" => {
                handle.turn_delivered.store(true, Ordering::Release);
            }
            "recent_stop" => {
                assert!(crate::db::cancel_tombstones::global_pool().is_none());
                crate::services::discord::tmux::tmux_kill_policy::record_recent_turn_stop(
                    f.channel,
                    Some(&f.session),
                    "fixture recent stop",
                )
                .await;
            }
            "direct_denial" => {
                let mut foreign = seed::row(&f, 0);
                foreign.turn_nonce = Some("foreign-B".into());
                foreign.set_relay_owner_kind(
                    crate::services::discord::inflight::RelayOwnerKind::Watcher,
                );
                save_inflight_state(&foreign).unwrap();
            }
            "wrong_source" => {
                std::fs::rename(&f.output, format!("{}.old", f.output)).unwrap();
                std::fs::write(&f.output, &bytes).unwrap();
            }
            _ => {}
        }
        barrier.release();
        let rejected_route = match mode {
            "pre_emit_reject" => Some("Late epoch/delivered guard: suppressed duplicate relay"),
            "recent_stop" => Some("after recent turn stop"),
            "relay_suppressed" => Some("route=\"suppressed\""),
            _ => None,
        };
        if let Some(route) = rejected_route {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            loop {
                let log = String::from_utf8_lossy(&LOG.lock().unwrap()[log_start..]).into_owned();
                if log.contains(route) {
                    eprintln!("REJECTION_REACHED mode={mode} route={route}");
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "actual rejection was not reached: {mode} log={log}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        let good = mode == "good" && receipt;
        if good {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while !c::trace(f.channel.get()).contains(&"normal_ack") {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "genuine C terminal was not consumed: trace={:?} source={:?}",
                    c::trace(f.channel.get()),
                    observed.snapshot()
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        } else if matches!(mode, "cas_miss" | "ack_lost" | "wrong_attempt") {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while !c::trace(f.channel.get()).contains(&"classification_returned") {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "actual C classification was not attempted: {mode} {:?}",
                    c::trace(f.channel.get())
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        } else if !matches!(
            mode,
            "cancel"
                | "authority"
                | "pre_emit_reject"
                | "wrong_source"
                | "recent_stop"
                | "direct_denial"
                | "relay_suppressed"
        ) {
            seed::until(&observed, "terminal consumer reached", |s| {
                if retired {
                    s.event_count("retired_terminal_consumed") == 1
                } else {
                    s.event_count("parsed_terminal") == 1
                }
            })
            .await;
            tokio::time::sleep(Duration::from_millis(150)).await;
        } else {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
    cancel_for_test(&handle.cancel, Ordering::Release);
    let joined = submissions.join_all().await;
    assert_eq!(joined.len(), 1);
    assert_eq!(
        joined[0].1,
        crate::services::discord::task_supervisor::watcher_completion::Outcome::Returned
    );
    let trace = c::trace(f.channel.get());
    if matches!(mode, "cas_miss" | "ack_lost" | "wrong_attempt") {
        assert_eq!(
            trace
                .iter()
                .filter(|event| **event == "classify_attempt")
                .count(),
            1
        );
    }
    let ack = trace.iter().filter(|event| **event == "normal_ack").count();
    assert_eq!(
        ack,
        usize::from(mode == "good" && receipt),
        "actual watcher terminal {mode} retired={retired}: {trace:?}"
    );
    if let Some(id) = id {
        let state = c::state(&pool, id).await;
        if mode == "good" || mode == "ack_lost" {
            assert_eq!(state, "classified_normal");
        } else if mode == "cas_miss" {
            assert_eq!(state, "withheld");
        } else {
            assert_eq!(state, "started_unclassified");
        }
        let source: (String, Vec<String>) = sqlx::query_as(
            "SELECT user_text, replay_source_message_ids FROM intake_outbox WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            source,
            (
                PROMPT.into(),
                vec![
                    format!("source-{}", f.channel.get()),
                    format!("absorbed-{}", f.channel.get())
                ]
            )
        );
        assert!(crate::services::discord::replay_policy::injected::protects(
            f.channel.get(),
            Some("B-owner")
        ));
    } else {
        assert!(
            trace.is_empty(),
            "receipt-less path must never enter C consumer"
        );
    }
    assert!(f.shared.tmux_watchers.len() <= owners);
    assert_eq!(
        submissions.submissions(),
        1,
        "C does not submit another output owner"
    );

    if matches!(
        mode,
        "cancel"
            | "authority"
            | "pre_emit_reject"
            | "wrong_source"
            | "recent_stop"
            | "direct_denial"
            | "relay_suppressed"
    ) {
        assert!(trace.is_empty(), "rejection precedes C consume: {trace:?}");
    }
    if mode == "good" && !receipt {
        let snapshot = observed.snapshot();
        assert_eq!(snapshot.event_count("parsed_terminal"), 1);
        assert_eq!(snapshot.event_count("post_stream_exit"), 1);
        if retired {
            assert_eq!(snapshot.retired_terminal_ranges, [(0, bytes.len() as u64)]);
        }
    }
    let requests = discord.requests_for(f.channel.get());
    if !receipt {
        eprintln!(
            "LEGACY_PHASE retired={retired} calls=terminal_consumer,post_stream_exit HTTP={} custody_ACK={} C=0",
            requests.len(),
            observed.snapshot().event_count("retired_terminal_consumed")
        );
    }
    eprintln!(
        "WATCHER case={case} retired={retired} mode={mode} receipt={receipt} trace={trace:?} observations={:?} http={requests:?}",
        observed.snapshot()
    );
    db.drop().await;
    (requests, trace)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_watcher_actual_terminal_matrix_pg() {
    if !child("injected_watcher_actual_terminal_matrix_pg") {
        return;
    }
    for (i, mode) in [
        "enqueue",
        "marker",
        "wrong_nonce",
        "wrong_hash",
        "wrong_session",
        "paste_only",
        "before_input",
        "cancel",
        "pre_emit_reject",
        "recent_stop",
        "direct_denial",
        "relay_suppressed",
        "cas_miss",
        "ack_lost",
        "wrong_attempt",
        "good",
    ]
    .iter()
    .enumerate()
    {
        case(i as u64, false, mode, true).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_retired_watcher_actual_terminal_matrix_pg() {
    if !child("injected_retired_watcher_actual_terminal_matrix_pg") {
        return;
    }
    for (i, mode) in [
        "enqueue",
        "wrong_nonce",
        "wrong_hash",
        "wrong_session",
        "paste_only",
        "cancel",
        "authority",
        "wrong_source",
        "cas_miss",
        "ack_lost",
        "wrong_attempt",
        "good",
    ]
    .iter()
    .enumerate()
    {
        case(100 + i as u64, true, mode, true).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_watcher_without_receipt_main_equivalence_pg() {
    if !child("injected_watcher_without_receipt_main_equivalence_pg") {
        return;
    }
    for (i, retired) in [false, true].into_iter().enumerate() {
        let (requests, trace) = case(200 + i as u64, retired, "good", false).await;
        assert!(trace.is_empty());
        if retired {
            assert!(requests.is_empty(), "O retains its independent transport");
        } else {
            assert_eq!(
                requests.len(),
                1,
                "main has one delivery with no extra HTTP calls"
            );
            assert!(requests[0].starts_with("POST "));
            let body: serde_json::Value =
                serde_json::from_str(requests[0].splitn(3, ' ').nth(2).unwrap()).unwrap();
            assert_eq!(body["content"], "C result");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_owner_retry_guard_pg() {
    if !child("injected_owner_retry_guard_pg") {
        return;
    }
    use crate::db::replay_disposition::write::{self, BeginFrom, CanonicalInput, EffectProjection};
    use crate::services::discord::replay_policy::live::{LiveAttempt, Seen, TerminalKind};
    use crate::services::discord::replay_policy::{self, StaleResume};
    use crate::services::provider::CancelToken;
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    for (i, stage) in ["reservation", "started"].iter().enumerate() {
        let f =
            InstallFixture::new_with_pool(CLAUDE, 600_840_000 + i as u64, Some(pool.clone())).await;
        bind(&f);
        let mut row = seed::row(&f, 0);
        row.turn_nonce = Some("B-owner".into());
        let token = CancelToken::from_persisted_turn_nonce(Some("B-owner".into()));
        let input = CanonicalInput {
            request_key: format!("B-quiet-{stage}"),
            provider: "claude".into(),
            channel: f.channel.to_string(),
            sources: vec![format!("B-{stage}")],
            original_text: "B quiet input".into(),
            owner_id: "B-user".into(),
            agent_id: "B-agent".into(),
            attachments: serde_json::json!([]),
            reply_context: None,
            provenance: "new_input",
        };
        let registered = write::register_or_reuse(&pool, &input, "fixture-node")
            .await
            .unwrap();
        let nonce = registered.episode_nonce.as_deref().unwrap();
        let binding = serde_json::json!({});
        let projection = EffectProjection {
            effect_target: "B",
            input_hash: "B-hash",
            binding: &binding,
        };
        let begun = write::begin(
            &pool,
            registered.id,
            BeginFrom::Registered { nonce },
            &projection,
            "B-incarnation",
        )
        .await
        .unwrap();
        let live = Arc::new(LiveAttempt::new(begun.into_parts().0));
        assert!(token.attach_replay_attempt(live.clone()));
        for fact in [
            Seen::StdoutEof,
            Seen::Exited { waited: true },
            Seen::StderrDrained,
            Seen::Terminal(TerminalKind::Error),
        ] {
            live.record(fact);
        }
        if *stage == "reservation" {
            c::reserve(f.channel.get(), "B-owner");
        } else {
            c::attach(
                &f.shared,
                c::Input {
                    channel: f.channel.get(),
                    pane: &f.session,
                    path: &f.output,
                    owner_nonce: "B-owner",
                    session: SESSION,
                    text: PROMPT,
                    frontier: 0,
                },
            )
            .await;
        }
        assert!(
            matches!(
                replay_policy::stale_resume(Some(&pool), &token, &row).await,
                StaleResume::Hold
            ),
            "C {stage} invalidates the old quiet B no-effect snapshot"
        );
        assert_eq!(c::state(&pool, registered.id).await, "started_unclassified");
        let current_nonce: String =
            sqlx::query_scalar("SELECT replay_episode_nonce FROM intake_outbox WHERE id=$1")
                .bind(registered.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(current_nonce, nonce, "no new retry nonce");
        let legacy = CancelToken::from_persisted_turn_nonce(Some("B-owner".into()));
        assert!(
            matches!(
                replay_policy::stale_resume(Some(&pool), &legacy, &row).await,
                StaleResume::Hold
            ),
            "legacy B cannot retry across C {stage}"
        );
        let unrelated = CancelToken::from_persisted_turn_nonce(Some("unrelated-B".into()));
        let mut unrelated_row = row.clone();
        unrelated_row.user_msg_id += 10;
        unrelated_row.turn_nonce = Some("unrelated-B".into());
        assert!(matches!(
            replay_policy::stale_resume(Some(&pool), &unrelated, &unrelated_row).await,
            StaleResume::Legacy
        ));
        eprintln!(
            "OWNER stage={stage} protected/existing/legacy B held; unrelated owner preserved"
        );
    }
    db.drop().await;
}
