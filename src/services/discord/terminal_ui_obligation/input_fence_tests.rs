//! Actual sweep coverage with Legacy host admission and input protection installed at waits.

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::host_defer_gate::tests::{
    Nameless, ScriptedTmux, map_channel, postgres,
};
use crate::services::discord::host_teardown_gate::test_support::{
    Stored, channel_key, seed, shared_on,
};
use crate::services::discord::input_runtime::fence::{self, Gate, Mode};
use crate::services::turn_orchestrator::QueuePersistenceContext;
use axum::{Json, Router, http::Method, http::StatusCode, http::Uri, response::IntoResponse};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy, Debug)]
enum Shape {
    Invalid,
    Stale,
    GenerationMismatch,
    MissingExpired,
    Complete,
    Deadline,
    RestError,
}

impl Shape {
    const ALL: [Self; 7] = [
        Self::Invalid,
        Self::Stale,
        Self::GenerationMismatch,
        Self::MissingExpired,
        Self::Complete,
        Self::Deadline,
        Self::RestError,
    ];
}

#[derive(Clone, Debug)]
struct Request {
    channel: u64,
    content: String,
}

struct MockRest {
    http: Arc<serenity::Http>,
    log: Arc<Mutex<Vec<Request>>>,
    entered: Arc<Notify>,
    release: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
}

impl MockRest {
    async fn new(latched: bool) -> Self {
        let log = Arc::new(Mutex::new(Vec::new()));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(if latched { 0 } else { 1000 }));
        let (recorded, signalled, response) = (log.clone(), entered.clone(), release.clone());
        let app = Router::new().fallback(
            move |method: Method, uri: Uri, Json(body): Json<serde_json::Value>| {
                let (log, entered, release) =
                    (recorded.clone(), signalled.clone(), response.clone());
                async move {
                    assert_eq!(method, Method::PATCH, "the real sweep edits through PATCH");
                    let parts: Vec<_> = uri.path().split('/').collect();
                    let channel_at = parts.iter().position(|part| *part == "channels").unwrap();
                    let channel: u64 = parts[channel_at + 1].parse().unwrap();
                    let id = parts.last().unwrap().parse::<u64>().unwrap();
                    let content = body["content"].as_str().unwrap().to_string();
                    log.lock().unwrap().push(Request {
                        channel,
                        content: content.clone(),
                    });
                    entered.notify_one();
                    release.acquire().await.unwrap().forget();
                    if content == "rest-error" {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "code": 50035, "message": "fixture edit rejection"
                            })),
                        )
                            .into_response();
                    }
                    Json(serde_json::json!({
                    "id": id.to_string(), "channel_id": channel.to_string(), "content": content,
                    "author": {"id":"1", "username":"bot", "discriminator":"0001", "avatar":null},
                    "timestamp":"2026-10-01T00:00:00+00:00", "edited_timestamp":null,
                    "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                    "attachments":[], "embeds":[], "pinned":false, "type":0
                })).into_response()
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            http,
            log,
            entered,
            release,
            task,
        }
    }

    fn requests(&self, channel: u64) -> Vec<Request> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.channel == channel)
            .cloned()
            .collect()
    }
}

impl Drop for MockRest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn obligation_root() -> PathBuf {
    runtime_store::discord_terminal_ui_obligations_root().expect("isolated obligation root")
}

fn obligation_bytes(channel: u64) -> Vec<u8> {
    fs::read(obligation_path_in_root(
        &obligation_root(),
        "claude",
        channel,
    ))
    .expect("kept obligation")
}

fn sample_obligation(channel: u64) -> TerminalUiObligation {
    TerminalUiObligation {
        channel_id: channel,
        provider: "claude".to_string(),
        status_message_id: 456,
        generation_mtime_ns: 789,
        pending_state_text: "pending".to_string(),
        completion_text: "complete".to_string(),
        deadline_text: "deadline".to_string(),
        created_unix: 1000,
        deadline_unix: 1600,
    }
}

async fn fixture(shared: &Arc<SharedData>, pool: &sqlx::PgPool, channel: u64, shape: Shape) {
    let name = ProviderKind::Claude.build_tmux_session_name(&format!("ui-input-fence-{channel}"));
    if matches!(shape, Shape::MissingExpired) {
        Nameless::Legacy
            .seed(
                pool,
                &shared.token_hash,
                "ui-fence-unused-bot",
                channel,
                &name,
            )
            .await;
    } else {
        seed(
            pool,
            &channel_key(shared, &name),
            &name,
            channel,
            Stored::Legacy,
        )
        .await;
    }
    let now = terminal_ui_obligation_now_unix();
    let mut obligation = TerminalUiObligation {
        channel_id: channel,
        provider: "claude".to_string(),
        status_message_id: channel + 10,
        generation_mtime_ns: 789,
        pending_state_text: "pending".to_string(),
        completion_text: "complete".to_string(),
        deadline_text: "deadline".to_string(),
        created_unix: now - 120,
        deadline_unix: now + 3600,
    };
    let mut state = None;
    if !matches!(shape, Shape::MissingExpired) {
        map_channel(
            shared,
            ChannelId::new(channel),
            &format!("ui-input-fence-{channel}"),
        )
        .await;
        let output = obligation_root().join(format!("{channel}.jsonl"));
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        let text = if matches!(shape, Shape::Deadline) {
            "{\"type\":\"user\",\"message\":{\"content\":\"busy\"}}\n"
        } else {
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"ui-fixture\"}\n"
        };
        fs::write(&output, text).unwrap();
        let generation = crate::services::tmux_common::session_temp_path(&name, "generation");
        fs::create_dir_all(Path::new(&generation).parent().unwrap()).unwrap();
        fs::write(generation, "1").unwrap();
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            &name,
            RuntimeHandoffKind::ClaudeTui,
        )
        .unwrap();
        let mut row = InflightTurnState::new(
            ProviderKind::Claude,
            channel,
            None,
            1,
            channel + 1,
            channel + 2,
            "terminal UI fixture".to_string(),
            None,
            Some(name),
            Some(output.to_string_lossy().into_owned()),
            None,
            0,
        );
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.status_message_id = Some(if matches!(shape, Shape::Stale) {
            channel + 11
        } else {
            channel + 10
        });
        row.last_watcher_relayed_generation_mtime_ns =
            Some(if matches!(shape, Shape::GenerationMismatch) {
                790
            } else {
                789
            });
        inflight::save_inflight_state_create_new(&row).expect("unprotected fixture row");
        state = Some(row);
    }
    if matches!(shape, Shape::Invalid) {
        obligation.status_message_id = 0;
    }
    if matches!(
        shape,
        Shape::MissingExpired | Shape::Deadline | Shape::RestError
    ) {
        obligation.deadline_unix = now - 120;
    }
    if matches!(shape, Shape::RestError) {
        obligation.completion_text = "rest-error".to_string();
    }
    assert!(
        !terminal_ui_host_deferred(
            shared,
            &ProviderKind::Claude,
            ChannelId::new(channel),
            state.as_ref()
        )
        .await,
        "Legacy host evidence must admit {shape:?}; input hold cannot be credited to the host guard"
    );
    write_obligation_in_root(&obligation_root(), &obligation).unwrap();
}

async fn protected_mode(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel: u64,
    mode: Mode,
) -> fence::test_health::Clear {
    let gate = Gate::protect(provider.clone(), channel).unwrap();
    let health = fence::test_health::Clear::new(&gate);
    if mode != Mode::LegacyOpen {
        let closing = Arc::new(gate.close().unwrap());
        if mode != Mode::Closing {
            let transition = shared
                .freeze_legacy_queue(
                    closing.clone(),
                    QueuePersistenceContext::new(&provider, "ui-fence", None),
                )
                .await
                .unwrap();
            match mode {
                Mode::Frozen => {}
                Mode::LedgerOpen => closing.open_ledger().unwrap(),
                Mode::Held => closing.hold().unwrap(),
                Mode::Handback => closing.begin_handback().unwrap(),
                Mode::LegacyOpen | Mode::Closing => unreachable!(),
            }
            drop(transition);
        }
    }
    assert_eq!(gate.mode(), mode);
    assert!(fence::boot_skip(fence::BootTarget::Channel(
        &ProviderKind::Claude,
        channel
    )));
    health
}

#[tokio::test]
async fn sweep_holds_every_protected_shape_and_processes_legacy_siblings_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let rest = MockRest::new(false).await;
    let modes = [
        Mode::LegacyOpen,
        Mode::Closing,
        Mode::Frozen,
        Mode::LedgerOpen,
        Mode::Handback,
        Mode::Held,
    ];
    let mut kept = Vec::new();
    let mut health = Vec::new();
    for (m, mode) in modes.into_iter().enumerate() {
        for (s, shape) in Shape::ALL.into_iter().enumerate() {
            let channel = 6_325_880_000 + (m * 100 + s) as u64;
            fixture(&shared, &pool, channel, shape).await;
            kept.push((channel, shape, mode, obligation_bytes(channel)));
            health.push(protected_mode(&shared, ProviderKind::Claude, channel, mode).await);
        }
    }
    let cross_provider = 6_325_880_700;
    fixture(&shared, &pool, cross_provider, Shape::Invalid).await;
    let cross_bytes = obligation_bytes(cross_provider);
    health.push(
        protected_mode(
            &shared,
            ProviderKind::Codex,
            cross_provider,
            Mode::LegacyOpen,
        )
        .await,
    );
    for (s, shape) in Shape::ALL.into_iter().enumerate() {
        fixture(&shared, &pool, 6_325_880_800 + s as u64, shape).await;
    }
    sweep_terminal_ui_obligations(&rest.http, &shared, &ProviderKind::Claude).await;
    for (channel, shape, mode, before) in kept {
        assert_eq!(
            obligation_bytes(channel),
            before,
            "{mode:?} {shape:?} obligation unchanged"
        );
        assert!(
            rest.requests(channel).is_empty(),
            "{mode:?} {shape:?} PATCH0"
        );
    }
    assert_eq!(
        obligation_bytes(cross_provider),
        cross_bytes,
        "other provider protects the same channel"
    );
    for (s, shape) in Shape::ALL.into_iter().enumerate() {
        let channel = 6_325_880_800 + s as u64;
        assert!(
            !obligation_path_in_root(&obligation_root(), "claude", channel).exists(),
            "unprotected {shape:?} follows existing clear"
        );
        let expected = match shape {
            Shape::Complete => Some("complete"),
            Shape::Deadline => Some("deadline"),
            Shape::RestError => Some("rest-error"),
            _ => None,
        };
        let requests = rest.requests(channel);
        assert_eq!(
            requests.len(),
            usize::from(expected.is_some()),
            "unprotected {shape:?} REST count"
        );
        if let Some(content) = expected {
            assert_eq!(requests[0].content, content);
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn sweep_unknown_channel_holds_while_legacy_sibling_runs_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let rest = MockRest::new(false).await;
    fixture(&shared, &pool, 6_325_881_000, Shape::Invalid).await;
    let unknown = TerminalUiObligation {
        channel_id: 0,
        ..sample_obligation(0)
    };
    write_obligation_in_root(&obligation_root(), &unknown).unwrap();
    let before = obligation_bytes(0);
    let _health = protected_mode(
        &shared,
        ProviderKind::Claude,
        6_325_881_001,
        Mode::LegacyOpen,
    )
    .await;
    let hold = fence::HoldUnknownForTest::new();
    assert!(fence::boot_skip(fence::BootTarget::Unknown));
    sweep_terminal_ui_obligations(&rest.http, &shared, &ProviderKind::Claude).await;
    assert_eq!(
        obligation_bytes(0),
        before,
        "channel=0 is Unknown, never a trusted channel"
    );
    assert!(
        !obligation_path_in_root(&obligation_root(), "claude", 6_325_881_000).exists(),
        "Legacy sibling still clears"
    );
    assert!(rest.log.lock().unwrap().is_empty());
    drop(hold);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn sweep_rechecks_protection_after_core_resolve_lock_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let rest = MockRest::new(false).await;
    let channel = 6_325_882_000;
    let name = ProviderKind::Claude.build_tmux_session_name("ui-input-core-wait");
    map_channel(&shared, ChannelId::new(channel), "ui-input-core-wait").await;
    seed(
        &pool,
        &channel_key(&shared, &name),
        &name,
        channel,
        Stored::Legacy,
    )
    .await;
    let generation = crate::services::tmux_common::session_temp_path(&name, "generation");
    fs::create_dir_all(Path::new(&generation).parent().unwrap()).unwrap();
    fs::write(generation, "2").unwrap();
    let obligation = TerminalUiObligation {
        channel_id: channel,
        generation_mtime_ns: 1,
        ..sample_obligation(channel)
    };
    write_obligation_in_root(&obligation_root(), &obligation).unwrap();
    fixture(&shared, &pool, channel + 1, Shape::Invalid).await;
    let before = obligation_bytes(channel);
    let core = shared.core.lock().await;
    let provider = ProviderKind::Claude;
    let sweep = sweep_terminal_ui_obligations(&rest.http, &shared, &provider);
    tokio::pin!(sweep);
    assert!(
        futures::poll!(sweep.as_mut()).is_pending(),
        "resolve blocks on the actual Core lock"
    );
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    drop(core);
    sweep.await;
    assert_eq!(
        obligation_bytes(channel),
        before,
        "post-resolve stale clear is held"
    );
    assert!(rest.requests(channel).is_empty());
    assert!(!obligation_path_in_root(&obligation_root(), "claude", channel + 1).exists());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn sweep_rechecks_protection_after_real_rate_limit_wait_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let rest = MockRest::new(false).await;
    let channel = 6_325_883_000;
    fixture(&shared, &pool, channel, Shape::Complete).await;
    fixture(&shared, &pool, channel + 1, Shape::Invalid).await;
    let before = obligation_bytes(channel);
    let initial = tokio::time::Instant::now();
    shared
        .api_timestamps
        .insert(ChannelId::new(channel), initial);
    let provider = ProviderKind::Claude;
    let sweep = sweep_terminal_ui_obligations(&rest.http, &shared, &provider);
    tokio::pin!(sweep);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                futures::poll!(sweep.as_mut()).is_pending(),
                "sweep must be held in rate-limit wait"
            );
            if *shared.api_timestamps.get(&ChannelId::new(channel)).unwrap() > initial {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("real rate-limit helper entered");
    assert!(
        rest.requests(channel).is_empty(),
        "no HTTP started at protection baseline"
    );
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = fence::test_health::Clear::new(&gate);
    sweep.await;
    assert_eq!(obligation_bytes(channel), before);
    assert!(
        rest.requests(channel).is_empty(),
        "post-rate-limit guard prevents PATCH"
    );
    assert!(!obligation_path_in_root(&obligation_root(), "claude", channel + 1).exists());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn sweep_rechecks_protection_after_http_response_success_and_error_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let rest = MockRest::new(true).await;
    for (offset, shape) in [Shape::Complete, Shape::RestError].into_iter().enumerate() {
        let channel = 6_325_884_000 + offset as u64 * 10;
        fixture(&shared, &pool, channel, shape).await;
        fixture(&shared, &pool, channel + 1, Shape::Invalid).await;
        let before = obligation_bytes(channel);
        let provider = ProviderKind::Claude;
        let sweep = sweep_terminal_ui_obligations(&rest.http, &shared, &provider);
        tokio::pin!(sweep);
        tokio::select! {
            _ = sweep.as_mut() => panic!("sweep finished before the mock response latch"),
            entered = tokio::time::timeout(Duration::from_secs(5), rest.entered.notified()) => entered.expect("mock received the real PATCH"),
        }
        assert_eq!(
            rest.requests(channel).len(),
            1,
            "one PATCH began before protection"
        );
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        rest.release.add_permits(1);
        sweep.await;
        assert_eq!(
            obligation_bytes(channel),
            before,
            "{shape:?}: no clear after the response await"
        );
        assert_eq!(
            rest.requests(channel).len(),
            1,
            "already-started PATCH is counted honestly"
        );
        assert!(!obligation_path_in_root(&obligation_root(), "claude", channel + 1).exists());
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn sweep_with_no_input_protection_keeps_existing_processing_pg() {
    const CHILD: &str = "AGENTDESK_UI_FENCE_ZERO_CHILD";
    if std::env::var_os(CHILD).is_none() {
        use base64::Engine;

        let name = format!(
            "{}::sweep_with_no_input_protection_keeps_existing_processing_pg",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("fresh zero-protection test process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "fresh child failed: {stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed; 0 failed; 0 ignored;"),
            "exactly one child test executed: {stdout}"
        );
        assert!(
            stderr.contains("UI_PROTECTION_ZERO_DONE"),
            "child completed all fixture assertions: {stderr}"
        );
        println!(
            "UI_PROTECTION_ZERO_CHILD_STDOUT_B64={}",
            base64::engine::general_purpose::STANDARD.encode(&output.stdout)
        );
        println!(
            "UI_PROTECTION_ZERO_CHILD_STDERR_B64={}",
            base64::engine::general_purpose::STANDARD.encode(&output.stderr)
        );
        println!("UI_PROTECTION_ZERO_CHILD_VERIFIED executed=1 ignored=0 completed=1");
        return;
    }
    assert!(
        !fence::any_protected(),
        "fresh process has an actually empty registry"
    );
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = ScriptedTmux::install();
    let (db, pool) = postgres().await;
    let shared = shared_on(&pool).await;
    let rest = MockRest::new(false).await;
    for (s, shape) in Shape::ALL.into_iter().enumerate() {
        fixture(&shared, &pool, 6_325_885_000 + s as u64, shape).await;
    }
    write_obligation_in_root(&obligation_root(), &sample_obligation(0)).unwrap();
    sweep_terminal_ui_obligations(&rest.http, &shared, &ProviderKind::Claude).await;
    assert!(!obligation_path_in_root(&obligation_root(), "claude", 0).exists());
    for (s, shape) in Shape::ALL.into_iter().enumerate() {
        let channel = 6_325_885_000 + s as u64;
        assert!(
            !obligation_path_in_root(&obligation_root(), "claude", channel).exists(),
            "protection0 {shape:?} still clears"
        );
        assert_eq!(
            rest.requests(channel).len(),
            usize::from(matches!(
                shape,
                Shape::Complete | Shape::Deadline | Shape::RestError
            )),
            "protection0 {shape:?} REST count"
        );
    }
    assert!(!fence::any_protected(), "sweep never activates protection");
    pool.close().await;
    db.drop().await;
    eprintln!("UI_PROTECTION_ZERO_DONE");
}
