//! Protected attempts driven through the real turn bridge: a stale resume, recovery sentinel or
//! terminal is decided before any reset, retry, body replacement or ordinary session effect.

use super::*;
use crate::db::replay_disposition::write::{self, BeginFrom, CanonicalInput, EffectProjection};
use crate::services::discord::replay_policy::RetryPermit;
use crate::services::discord::replay_policy::live::{Activity, LiveAttempt, Seen, TerminalKind};
use crate::services::discord::turn_bridge::{
    BridgeCompletionSignal, TurnBridgeContext, spawn_turn_bridge,
};
use crate::services::memory::TokenUsage;
use poise::serenity_prelude::UserId;

const STALE: &str = "Error: No conversation found with session ID stale-sid";
const COMPLETE: [Seen; 3] = [
    Seen::StdoutEof,
    Seen::Exited { waited: true },
    Seen::StderrDrained,
];

/// Every retry the bridge schedules, legacy or exact, and every body it shows.
#[derive(Default)]
struct Effects {
    legacy: AtomicUsize,
    exact: Mutex<Vec<(i64, String)>>,
    shown: Mutex<Vec<String>>,
}

struct EffectGateway(Arc<Effects>);

impl EffectGateway {
    fn show(&self, content: &str) {
        self.0.shown.lock().unwrap().push(content.to_string());
    }
}

impl TurnGateway for EffectGateway {
    fn send_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        self.show(content);
        Box::pin(async { Ok(MessageId::new(DRIVER_FALLBACK_ANCHOR_MSG_ID)) })
    }

    fn edit_message<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.show(content);
        Box::pin(async { Ok(()) })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        _channel_id: ChannelId,
        _message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        self.show(content);
        Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        self.0.legacy.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }

    fn schedule_exact_retry_with_history<'a>(
        &'a self,
        _channel_id: ChannelId,
        _user_message_id: MessageId,
        _user_text: &'a str,
        retry: RetryPermit,
        completion_tx: tokio::sync::oneshot::Sender<()>,
    ) -> GatewayFuture<'a, ()> {
        let previous = retry.previous_nonce().to_string();
        self.0
            .exact
            .lock()
            .unwrap()
            .push((retry.receipt_id(), previous));
        let _ = completion_tx.send(());
        Box::pin(async {})
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        _channel_id: ChannelId,
        _intervention: &'a Intervention,
        _request_owner_name: &'a str,
        _has_more_queued_turns: bool,
        _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        true
    }

    fn can_deliver_directly(&self) -> bool {
        true
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

/// Installs a Discord REST mock for this thread that records every request body.
async fn rest_bodies() -> (
    Arc<Mutex<Vec<String>>>,
    crate::services::discord::shared_state::test_rest::Guard,
) {
    let bodies = Arc::<Mutex<Vec<String>>>::default();
    let recorded = bodies.clone();
    let app = axum::Router::new().fallback(move |body: String| {
        let recorded = recorded.clone();
        async move {
            recorded.lock().unwrap().push(body);
            axum::Json(serde_json::json!({
                "id": "4700001", "channel_id": DRIVER_CHANNEL_ID.to_string(), "content": "",
                "author": {"id":"1", "username":"bot", "discriminator":"0001", "avatar":null},
                "timestamp":"2026-10-01T00:00:00+00:00", "edited_timestamp":null,
                "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                "attachments":[], "embeds":[], "pinned":false, "type":0
            }))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = poise::serenity_prelude::HttpBuilder::new("test-token")
        .proxy(format!("http://{}", listener.local_addr().unwrap()))
        .ratelimiter_disabled(true)
        .build();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let guard = crate::services::discord::shared_state::test_rest::install(Arc::new(http));
    (bodies, guard)
}

/// What one bridge turn left behind.
#[derive(Debug)]
struct TurnEnd {
    disposition: Option<String>,
    hold_reason: Option<String>,
    replay_rows: i64,
    legacy: usize,
    exact: Vec<(i64, String)>,
    shown: Vec<String>,
    core_sid: Option<String>,
    killed: bool,
    barred: bool,
    started: Option<(i64, String)>,
}

/// How the turn's request is protected.
enum Request<'a> {
    Legacy,
    /// A started attempt whose live reader saw these facts.
    Protected(&'a [Seen]),
    /// A started attempt whose receipt another writer already held.
    ProtectedHeldElsewhere(&'a [Seen]),
}

/// A user message of its own per case: a held request bars its source for the whole process.
fn case_message(case: &str) -> u64 {
    6_008_000_000
        + case
            .bytes()
            .fold(0u64, |hash, byte| hash * 31 + u64::from(byte))
            % 1_000_000
}

async fn start_attempt(pool: &sqlx::PgPool, case: &str) -> write::ExactAttempt {
    let request = CanonicalInput {
        request_key: format!("replay-turn-{case}"),
        provider: "claude".into(),
        channel: DRIVER_CHANNEL_ID.to_string(),
        sources: vec![case_message(case).to_string()],
        original_text: "driver prompt".into(),
        owner_id: "1".into(),
        agent_id: "agent".into(),
        attachments: serde_json::json!([]),
        reply_context: None,
        provenance: "new_input",
    };
    let receipt = write::register_or_reuse(pool, &request, "node-a").await;
    let receipt = receipt.unwrap();
    let nonce = receipt.episode_nonce.unwrap();
    let binding = serde_json::json!({});
    let effect = EffectProjection {
        effect_target: "provider-start",
        input_hash: "input",
        binding: &binding,
    };
    let from = BeginFrom::Registered { nonce: &nonce };
    let ack = write::begin(pool, receipt.id, from, &effect, "incarnation-a").await;
    ack.unwrap().into_parts().0
}

/// Runs one turn through `spawn_turn_bridge` with `messages`, then waits for scheduled retries.
async fn bridge_turn(case: &str, request: Request<'_>, messages: Vec<StreamMessage>) -> TurnEnd {
    bridge_turn_with(case, request, messages, |_| {}).await
}

async fn bridge_turn_with(
    case: &str,
    request: Request<'_>,
    messages: Vec<StreamMessage>,
    prepare: impl FnOnce(&mut InflightTurnState),
) -> TurnEnd {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let mut driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 1);
    let (rest, _rest) = rest_bodies().await;
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    Arc::get_mut(&mut driver.shared).unwrap().pg_pool = Some(pool.clone());
    let shared = driver.shared.clone();
    let channel = ChannelId::new(DRIVER_CHANNEL_ID);
    let user = MessageId::new(case_message(case));
    // The bridge itself owns the turn's output, so every body it shows reaches this gateway.
    shared.tmux_watchers.remove(&channel);
    let mut session = resumable_session();
    session.channel_name = Some(format!("replay-turn-{case}"));
    shared.core.lock().await.sessions.insert(channel, session);
    let key = crate::services::discord::adk_session::build_adk_session_key(
        &shared,
        channel,
        &ProviderKind::Claude,
        None,
    );
    let key = key.await.expect("channel session key");
    crate::services::discord::inflight::seed_session_row_keyed(
        &pool,
        &key,
        DRIVER_CHANNEL_ID,
        None,
    )
    .await;
    let exit_reason =
        crate::services::tmux_common::session_temp_path(DRIVER_TMUX_SESSION, "exit_reason");
    let _ = std::fs::remove_file(&exit_reason);

    let cancel = Arc::new(CancelToken::new());
    cancel.bind_unmanaged_session_name(DRIVER_TMUX_SESSION);
    let mut started = None;
    if let Request::Protected(seen) | Request::ProtectedHeldElsewhere(seen) = request {
        let attempt = start_attempt(&pool, case).await;
        started = Some((attempt.receipt_id(), attempt.nonce().to_string()));
        if matches!(request, Request::ProtectedHeldElsewhere(_)) {
            let preserved = serde_json::json!({ "hold": "elsewhere" });
            write::withhold(&pool, &attempt, "held elsewhere", &preserved)
                .await
                .unwrap();
        }
        let live = LiveAttempt::new(attempt);
        for fact in seen {
            live.record(*fact);
        }
        assert!(cancel.attach_replay_attempt(Arc::new(live)));
    }
    driver.inflight.user_msg_id = user.get();
    prepare(&mut driver.inflight);
    crate::services::discord::inflight::save_inflight_state(&driver.inflight).unwrap();
    let started_turn = crate::services::discord::mailbox_try_start_turn(
        &shared,
        channel,
        cancel.clone(),
        UserId::new(1),
        user,
    );
    assert!(started_turn.await);

    let effects = Arc::new(Effects::default());
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<BridgeCompletionSignal>();
    let ctx = TurnBridgeContext {
        provider: ProviderKind::Claude,
        gateway: Arc::new(EffectGateway(effects.clone())),
        channel_id: channel,
        user_msg_id: Some(user),
        user_text_owned: "driver prompt".into(),
        request_owner_name: String::new(),
        role_binding: None,
        adk_session_key: Some(key),
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: Some(MessageId::new(DRIVER_CURRENT_MSG_ID)),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: Some(completion_tx),
        is_external_input_tui_direct: false,
        inflight_state: driver.inflight.clone(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    for message in messages {
        tx.send(message).unwrap();
    }
    drop(tx);
    spawn_turn_bridge(shared.clone(), cancel, rx, ctx);
    tokio::time::timeout(std::time::Duration::from_secs(30), completion_rx)
        .await
        .expect("real bridge postlude finishes")
        .expect("completion signal");
    // Retry scheduling runs on spawned tasks; a turn that scheduled nothing never spawns one.
    for _ in 0..100 {
        let exact = effects.exact.lock().unwrap().len();
        if effects.legacy.load(Ordering::SeqCst) + exact > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let receipt = started.as_ref().map_or(0, |(id, _)| *id);
    let stored: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT replay_disposition, replay_hold_reason FROM intake_outbox WHERE id = $1",
    )
    .bind(receipt)
    .fetch_optional(&pool)
    .await
    .unwrap();
    let replay_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM intake_outbox WHERE replay_disposition IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let (disposition, hold_reason) = stored.unwrap_or_default();
    let core_sid = shared.core.lock().await.sessions[&channel]
        .session_id
        .clone();
    let end = TurnEnd {
        disposition,
        hold_reason,
        replay_rows,
        legacy: effects.legacy.load(Ordering::SeqCst),
        exact: effects.exact.lock().unwrap().clone(),
        shown: [
            effects.shown.lock().unwrap().clone(),
            rest.lock().unwrap().clone(),
        ]
        .concat(),
        core_sid,
        killed: std::path::Path::new(&exit_reason).exists(),
        barred: crate::services::discord::replay_policy::legacy_retry_barred(
            DRIVER_CHANNEL_ID,
            user.get(),
        ),
        started,
    };
    pool.close().await;
    db.drop().await;
    drop(driver);
    end
}

fn seen(extra: &[Seen]) -> Vec<Seen> {
    COMPLETE.iter().chain(extra).copied().collect()
}

fn text(content: &str) -> StreamMessage {
    StreamMessage::Text {
        content: content.into(),
    }
}

fn error(message: &str) -> StreamMessage {
    StreamMessage::Error {
        message: message.into(),
        stdout: String::new(),
        stderr: String::new(),
        exit_code: Some(1),
    }
}

fn assert_held(end: &TurnEnd, reason: &str) {
    assert_eq!(end.disposition.as_deref(), Some("withheld"), "{end:?}");
    assert_eq!(end.hold_reason.as_deref(), Some(reason), "{end:?}");
    assert_eq!(
        (end.legacy, end.exact.len()),
        (0, 0),
        "nothing reruns: {end:?}"
    );
    assert_eq!(
        end.core_sid.as_deref(),
        Some("sid-core"),
        "session kept: {end:?}"
    );
    assert!(!end.killed && end.barred, "{end:?}");
}

// A stale resume after visible output, alone or mixed with a session-reset marker, keeps the
// partial body and session and never reaches provider recovery, guidance or a retry.
#[tokio::test]
async fn stale_after_text_preserves_body_without_replay_pg() {
    let mixed = format!("{STALE} [stream-json-no-output] quota exceeded");
    for (case, message) in [("text", STALE.to_string()), ("mixed", mixed)] {
        let facts = seen(&[
            Seen::Activity(Activity::Output),
            Seen::Terminal(TerminalKind::Error),
        ]);
        let messages = vec![text("partial answer"), error(&message)];
        let end = bridge_turn(case, Request::Protected(&facts), messages).await;
        assert_held(&end, "activity after start");
        assert_eq!(
            end.shown,
            ["partial answer"],
            "no guidance replaces the body: {end:?}"
        );
    }
}

// Only a quiet complete attempt retries, through its exact continuation alone: no reset, kill,
// session clear or legacy retry, and a later error in the turn cannot classify it ordinary.
#[tokio::test]
async fn stale_no_effect_attempt_retries_only_through_its_exact_permit_pg() {
    let facts = seen(&[Seen::Terminal(TerminalKind::Error)]);
    for (case, messages) in [
        ("exact", vec![error(STALE)]),
        ("exact-exit", vec![error(STALE), error("process exited")]),
    ] {
        let end = bridge_turn(case, Request::Protected(&facts), messages).await;
        assert_eq!(
            end.disposition.as_deref(),
            Some("startup_failed_no_effect"),
            "{case}: {end:?}"
        );
        assert_eq!(
            end.exact,
            end.started.clone().into_iter().collect::<Vec<_>>(),
            "{case}: {end:?}"
        );
        assert_eq!(end.legacy, 0, "{case}: {end:?}");
        assert_eq!(end.core_sid.as_deref(), Some("sid-core"), "{case}: {end:?}");
        assert!(!end.killed && end.barred, "{case}: {end:?}");
    }
}

// The empty-response branches judge the live observation, not the missing Init or the stale
// marker in the output file: a tool run holds, and only a quiet attempt is retried exactly.
#[tokio::test]
async fn quick_empty_resume_after_tool_is_withheld_pg() {
    let tool = seen(&[Seen::Activity(Activity::Tool)]);
    let end = bridge_turn("quick-tool", Request::Protected(&tool), Vec::new()).await;
    assert_held(&end, "activity after start");

    let quiet = seen(&[]);
    let end = bridge_turn("quick-quiet", Request::Protected(&quiet), Vec::new()).await;
    assert_eq!(
        end.disposition.as_deref(),
        Some("startup_failed_no_effect"),
        "{end:?}"
    );
    assert_eq!((end.legacy, end.exact.len()), (0, 1), "{end:?}");
    assert!(!end.killed, "{end:?}");

    let stale_file = |inflight: &mut InflightTurnState| {
        let path = std::env::temp_dir().join(format!("replay-turn-{}.jsonl", std::process::id()));
        let line = r#"{"type":"result","is_error":true,"result":"Error: No conversation found"}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();
        inflight.output_path = Some(path.display().to_string());
        inflight.last_offset = 0;
    };
    let tool = seen(&[Seen::Activity(Activity::Tool)]);
    let end = bridge_turn_with("scan", Request::Protected(&tool), Vec::new(), stale_file).await;
    assert_held(&end, "activity after start");
}

// The recovery sentinel is never no-effect evidence: even a quiet attempt is held, with no
// continue notice and no retry.
#[tokio::test]
async fn recovered_started_attempt_cannot_bypass_replay_hold_pg() {
    let quiet = seen(&[]);
    let died = StreamMessage::Done {
        result: "__session_died_retry__".into(),
        session_id: None,
    };
    let end = bridge_turn(
        "sentinel",
        Request::Protected(&quiet),
        vec![text("partial"), died],
    )
    .await;
    assert_held(&end, "recovery sentinel after start");
    assert_eq!(
        end.shown,
        ["partial"],
        "the body stays with no continue notice: {end:?}"
    );
}

// Ordinary completion follows only a committed ordinary classification of the provider's own
// terminal; a wrapper error, a second terminal or a lost classification holds instead.
#[tokio::test]
async fn normal_terminal_is_classified_before_ordinary_completion_pg() {
    let done = |sid: &str| StreamMessage::Done {
        result: String::new(),
        session_id: Some(sid.into()),
    };
    let ordinary = seen(&[
        Seen::Activity(Activity::Output),
        Seen::Terminal(TerminalKind::Done),
    ]);
    let messages = vec![text("answer"), done("sid-next")];
    let end = bridge_turn("done", Request::Protected(&ordinary), messages).await;
    assert_eq!(
        end.disposition.as_deref(),
        Some("classified_normal"),
        "{end:?}"
    );
    assert_eq!(
        end.core_sid.as_deref(),
        Some("sid-next"),
        "ordinary writeback: {end:?}"
    );
    assert_eq!(end.shown, ["answer"], "{end:?}");
    assert!(!end.barred, "{end:?}");

    let twice = seen(&[
        Seen::Activity(Activity::Output),
        Seen::Terminal(TerminalKind::Done),
        Seen::Terminal(TerminalKind::Error),
    ]);
    let messages = vec![text("answer"), done("sid-next")];
    let end = bridge_turn("twice", Request::Protected(&twice), messages).await;
    assert_held(&end, "activity after start");

    // A wrapper error is not a provider terminal: no guidance replaces the body.
    let wrapper = seen(&[]);
    let messages = vec![error("start permit refused before launch")];
    let end = bridge_turn("wrapper", Request::Protected(&wrapper), messages).await;
    assert_held(&end, "incomplete or unclassified observation");
    assert!(
        end.shown.is_empty(),
        "no guidance replaces the body: {end:?}"
    );

    let messages = vec![text("answer"), done("sid-next")];
    let held = Request::ProtectedHeldElsewhere(&ordinary);
    let end = bridge_turn("cas-miss", held, messages).await;
    assert_eq!(end.disposition.as_deref(), Some("withheld"), "{end:?}");
    assert_eq!(
        end.hold_reason.as_deref(),
        Some("held elsewhere"),
        "{end:?}"
    );
    assert_eq!(end.core_sid.as_deref(), Some("sid-core"), "{end:?}");
}

// Without a protected attempt the bridge keeps today's path: the legacy reset and history
// retry run and no replay row is written.
#[tokio::test]
async fn producer_dormant_keeps_legacy_execution_paths_pg() {
    let end = bridge_turn("legacy", Request::Legacy, vec![error(STALE)]).await;
    assert_eq!((end.legacy, end.exact.len()), (1, 0), "{end:?}");
    assert_eq!(end.replay_rows, 0, "{end:?}");
    assert_eq!(
        end.core_sid, None,
        "legacy reset clears the session: {end:?}"
    );
    assert!(
        end.shown
            .iter()
            .any(|body| body.contains("자동으로 이어갑니다")),
        "{end:?}"
    );
    assert!(end.killed && !end.barred, "{end:?}");
}

// Once a protected request is held, the shared history retry that a watcher or any other caller
// schedules refuses it before storing history or queueing the original text again.
#[tokio::test]
async fn held_request_refuses_the_legacy_history_retry_pg() {
    use crate::services::discord::turn_bridge::recovery_text::{
        auto_retry_with_history, retry_pending_contains,
    };
    let facts = seen(&[Seen::Activity(Activity::Output)]);
    let messages = vec![text("partial"), error(STALE)];
    let end = bridge_turn("history", Request::Protected(&facts), messages).await;
    assert!(end.barred, "{end:?}");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = poise::serenity_prelude::Http::new("");
    let channel = ChannelId::new(DRIVER_CHANNEL_ID);
    let user = MessageId::new(case_message("history"));
    let retry = auto_retry_with_history(&http, &shared, &ProviderKind::Claude, channel, user, "x");
    tokio::time::timeout(std::time::Duration::from_secs(5), retry)
        .await
        .expect("a held request returns before any Discord fetch");
    assert!(!retry_pending_contains(channel));
}
