//! A non-forced cancel through the production router takes a live Herdr turn's own `/stop` on the
//! token it judged, and completes nothing; every other turn and any forced cancel keep their path.
use super::*;
use crate::services::discord::commands::stop::{remote_cancel_reply, run_slash_stop};
use crate::services::discord::turn_bridge::tmux_runtime::judged_stop::AFTER_HERDR_JUDGE;
use crate::services::provider::cancel_token_claude_interrupt::HERDR_CANCEL_OVERRIDE;

const AUTH: &str = "herdr-http-cancel-token";
const NODE_ID: &str = "mini-http";

/// The production router's cancel of `channel` on a runtime serving `case`'s provider.
async fn cancel_on(case: &Case, channel: u64, force: bool, forwarded: bool) -> (StatusCode, Value) {
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    registry
        .register(case.provider.as_str().into(), case.shared.clone())
        .await;
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some(AUTH.into());
    let pool = case.shared.pg_pool.clone();
    let app = crate::server::routes::home_stop_test_app(config, pool, Some(registry), NODE_ID);
    let bearer = format!("Bearer {AUTH}");
    let mut headers = vec![("authorization", bearer.as_str())];
    if forwarded {
        headers.extend([
            ("x-agentdesk-forwarded-by", "gw"),
            ("x-agentdesk-session-owner", NODE_ID),
        ]);
    }
    let (uri, body) = (format!("/turns/{channel}/cancel"), json!({"force": force}));
    crate::server::routes::home_stop_test_post(&app, &uri, &headers, &body.to_string()).await
}

async fn cancel(case: &Case, force: bool, forwarded: bool) -> (StatusCode, Value) {
    cancel_on(case, case.channel.get(), force, forwarded).await
}

/// The holder fields and completion claims a Herdr cancel answers with.
fn claims(answer: &Value) -> Value {
    let keys = [
        "outcome",
        "intent",
        "delivery",
        "reason",
        "settlement",
        "effect_started",
        "turn_status",
        "turn_completed_at",
        "inflight_cleared",
        "dispatch_cancelled",
        "mailbox_foreground_free",
    ];
    let absent = || json!("<absent>");
    let field = |key: &str| {
        (
            key.to_owned(),
            answer.get(key).cloned().unwrap_or_else(absent),
        )
    };
    Value::Object(keys.into_iter().map(field).collect())
}

/// What a Herdr cancel claims for `intent`/`delivery`: a stop pending on the turn, never its end.
fn expected(intent: &str, delivery: &str, reason: Option<&str>) -> Value {
    let stopping = intent != "refused";
    json!({
        "outcome": "herdr",
        "intent": intent,
        "delivery": delivery,
        "reason": reason,
        "settlement": (intent == "recorded").then_some("host_owned"),
        "effect_started": delivery != "not_sent",
        "turn_status": if stopping { "stopping" } else { "refused" },
        "turn_completed_at": null,
        "inflight_cleared": false,
        "dispatch_cancelled": null,
        "mailbox_foreground_free": stopping.then_some(false),
    })
}

/// The session row's status and dispatch, which only the existing cancel's tail rewrites.
async fn row(case: &Case) -> (String, Option<String>) {
    let pool = case.shared.pg_pool.as_ref().unwrap();
    let row =
        sqlx::query_as("SELECT status, active_dispatch_id FROM sessions WHERE channel_id = $1")
            .bind(case.channel.to_string());
    row.fetch_one(pool).await.unwrap()
}

fn active_row() -> (String, Option<String>) {
    ("turn_active".into(), Some("dispatch-http".into()))
}

fn intent(token: &CancelToken) -> bool {
    let state = token.herdr_interrupt_state();
    state.is_some_and(|state| state.user_stop.load(Ordering::SeqCst))
}

/// A live Herdr turn whose session row is the channel's active, dispatched turn on this node.
fn live(
    pool: &sqlx::PgPool,
    root: &Path,
    rt: &tokio::runtime::Runtime,
    p: ProviderKind,
    n: u64,
) -> Case {
    let case = rt.block_on(Case::new(pool, root, p, n));
    let update = sqlx::query("UPDATE sessions SET status = 'turn_active', instance_id = $2, active_dispatch_id = 'dispatch-http' WHERE channel_id = $1")
        .bind(case.channel.to_string())
        .bind(NODE_ID);
    rt.block_on(update.execute(pool)).unwrap();
    case
}

/// One live turn per provider on channels no other stop test touches; `tmux_free` when no cancel
/// may fall to the tmux host.
fn with_live(
    first: u64,
    tmux_free: bool,
    mut check: impl FnMut(&sqlx::PgPool, &Path, &tokio::runtime::Runtime, ProviderKind, u64),
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    let db = runtime.block_on(crate::db::auto_queue::test_support::TestPostgresDb::create());
    let pool = runtime.block_on(db.connect_and_migrate());
    for (n, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        check(
            &pool,
            root.path(),
            &runtime,
            provider,
            first + 10 * n as u64,
        );
    }
    assert!(
        !tmux_free || fx.take_calls().is_empty(),
        "a Herdr cancel makes no tmux call"
    );
    runtime.block_on(pool.close());
    runtime.block_on(db.drop());
}

async fn slash_stop(case: &Case) -> &'static str {
    let reply = run_slash_stop(&case.shared, &case.provider, case.channel).await;
    let text = reply.text();
    reply
        .finish(&case.shared, &case.provider, case.channel)
        .await;
    text
}

fn already() -> &'static str {
    HerdrStop::AlreadyRequested.reply()
}

// A non-forced cancel of a live Herdr turn sends its one Escape and answers as the holder: the
// turn, its dispatch and its row stay for the provider's own end; a forced cancel keeps its refusal.
#[test]
fn herdr_http_cancel_stops_a_live_turn_and_completes_nothing_pg() {
    with_live(820, true, |pool, root, runtime, provider, n| {
        let case = live(pool, root, runtime, provider, n);
        let _registry = case.rig.registry_on_this_thread();
        let (status, _) = runtime.block_on(cancel(&case, true, false));
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a forced cancel keeps the host guard"
        );
        assert_eq!((case.escapes(), intent(&case.token)), (0, false));
        let (status, answer) = runtime.block_on(cancel(&case, false, true));
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(claims(&answer), expected("recorded", "sent", None));
        assert_eq!(
            remote_cancel_reply(&answer),
            "holder 노드가 중지 키를 보냈어요. 작업 종료 기록을 기다려요."
        );
        assert_eq!((case.escapes(), intent(&case.token)), (1, true));
        assert!(!case.token.cancelled.load(Ordering::SeqCst));
        assert!(current_token(&case, runtime).is_some_and(|t| Arc::ptr_eq(&t, &case.token)));
        assert_eq!(runtime.block_on(row(&case)), active_row());
        // A later `/stop` and cancel share the intent: neither sends another key.
        assert_eq!(runtime.block_on(slash_stop(&case)), already());
        let (_, again) = runtime.block_on(cancel(&case, false, false));
        assert_eq!(
            claims(&again),
            expected("already_recorded", "not_sent", Some("duplicate"))
        );
        assert_eq!(case.escapes(), 1);
        assert_eq!(runtime.block_on(row(&case)), active_row());
    });
}

// With the Escape switch off the holder answers `switch_off`: no intent, no key, and the remote
// `/stop` says so instead of "stopping".
#[test]
fn herdr_http_cancel_with_the_switch_off_records_and_sends_nothing_pg() {
    with_live(860, true, |pool, root, runtime, provider, n| {
        let case = live(pool, root, runtime, provider, n);
        let _registry = case.rig.registry_on_this_thread();
        HERDR_CANCEL_OVERRIDE.set(Some(false));
        let (status, answer) = runtime.block_on(cancel(&case, false, true));
        HERDR_CANCEL_OVERRIDE.set(Some(true));
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(
            claims(&answer),
            expected("refused", "not_sent", Some("switch_off"))
        );
        assert_eq!(
            remote_cancel_reply(&answer),
            "holder 노드가 중지 키를 보내지 않았어요 (switch_off)."
        );
        assert_eq!((case.escapes(), intent(&case.token)), (0, false));
        assert!(!case.token.cancelled.load(Ordering::SeqCst));
        assert_eq!(runtime.block_on(row(&case)), active_row());
    });
}

// A cancel after `/stop`, or racing it, takes the same token's intent and write claim: one Escape.
#[test]
fn herdr_http_cancel_and_slash_stop_share_one_intent_and_one_escape_pg() {
    with_live(900, true, |pool, root, runtime, provider, n| {
        {
            let case = live(pool, root, runtime, provider.clone(), n);
            let _registry = case.rig.registry_on_this_thread();
            assert_eq!(runtime.block_on(slash_stop(&case)), sent_reply());
            let (status, answer) = runtime.block_on(cancel(&case, false, false));
            assert_eq!(status, StatusCode::OK, "{answer}");
            let duplicate = expected("already_recorded", "not_sent", Some("duplicate"));
            assert_eq!(claims(&answer), duplicate);
            assert_eq!(case.escapes(), 1);
        }
        let case = live(pool, root, runtime, provider, n + 1);
        let _registry = case.rig.registry_on_this_thread();
        let (http, slash) = runtime
            .block_on(async { tokio::join!(cancel(&case, false, false), slash_stop(&case)) });
        let intents = (http.1["intent"].as_str(), slash == already());
        assert!(
            matches!(
                intents,
                (Some("recorded"), true) | (Some("already_recorded"), false)
            ),
            "one of the two records the intent: {intents:?} {}",
            http.1
        );
        assert_eq!((case.escapes(), intent(&case.token)), (1, true));
    });
}

fn sent_reply() -> &'static str {
    HerdrStop::Requested(HerdrDelivery::Sent).reply()
}

/// A successor turn on `case`'s pane, prepared as its own start leaves it.
fn successor(case: &Case) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    let observation = token.prepare_herdr_interrupt(case.provider.clone(), &case.owner);
    observation.submission.lock().unwrap().submission = HerdrSubmission::Submitted;
    if case.provider == ProviderKind::Codex {
        let meta = std::fs::metadata(&case.path).unwrap();
        let file = crate::services::tui_o::shadow::capture::file_identity(&meta);
        assert!(observation.record_turn_start(HerdrTurnStart {
            execution_nonce: case.record.execution_nonce.clone(),
            source: case.path.clone(),
            file: Some(file),
            offset: 0,
            submitted_at: None,
        }));
    }
    token
}

// The judged turn ends and a successor starts before the stop runs: the stop refuses the judged
// token, and the successor gets no key, no intent and no row change.
#[test]
fn herdr_http_cancel_never_reaches_a_successor_started_after_its_judge_pg() {
    with_live(940, true, |pool, root, runtime, provider, n| {
        let case = live(pool, root, runtime, provider, n);
        let _registry = case.rig.registry_on_this_thread();
        let next = successor(&case);
        let (shared, provider, channel) =
            (case.shared.clone(), case.provider.clone(), case.channel);
        let started = next.clone();
        let step = async move {
            crate::services::discord::mailbox_finish_turn(&shared, &provider, channel).await;
            let message = MessageId::new(channel.get() + 2);
            let start = crate::services::discord::mailbox_try_start_turn;
            assert!(start(&shared, channel, started, UserId::new(7), message).await);
        };
        AFTER_HERDR_JUDGE.with_borrow_mut(|after| *after = Some(Box::pin(step)));
        let (status, answer) = runtime.block_on(cancel(&case, false, false));
        let ran = AFTER_HERDR_JUDGE.with_borrow(Option::is_none);
        assert!(ran, "the successor started between the judge and the stop");
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(
            claims(&answer),
            expected("refused", "not_sent", Some("generation"))
        );
        assert_eq!(case.escapes(), 0, "no key reaches the successor");
        assert_eq!((intent(&next), intent(&case.token)), (false, false));
        assert!(!next.cancelled.load(Ordering::SeqCst));
        assert!(current_token(&case, runtime).is_some_and(|t| Arc::ptr_eq(&t, &next)));
        assert_eq!(runtime.block_on(row(&case)), active_row());
    });
}

// A Herdr turn restored without this process's state keeps the existing refusal, and a channel
// with no Herdr turn keeps the existing cancel, whose remote `/stop` answer stays "stopping".
#[test]
fn herdr_http_cancel_keeps_the_existing_cancel_for_other_turns_pg() {
    with_live(980, false, |pool, root, runtime, provider, n| {
        let case = live(pool, root, runtime, provider, n);
        let _registry = case.rig.registry_on_this_thread();
        // Restored after a restart: bound to its Herdr session, with none of this process's state.
        let restored = Arc::new(CancelToken::new());
        let name = case.owner.logical_key.clone();
        let binding =
            crate::services::provider::cancel_token_cleanup::authority::TmuxBinding::NameOnly {
                name,
            };
        *restored.tmux_binding.lock().unwrap() = Some(binding);
        runtime.block_on(async {
            let (shared, channel) = (&case.shared, case.channel);
            crate::services::discord::mailbox_finish_turn(shared, &case.provider, channel).await;
            let message = MessageId::new(channel.get() + 2);
            let start = crate::services::discord::mailbox_try_start_turn;
            assert!(start(shared, channel, restored.clone(), UserId::new(7), message).await);
        });
        let (status, answer) = runtime.block_on(cancel(&case, false, false));
        assert_eq!(status, StatusCode::CONFLICT, "{answer}");
        assert_eq!(case.escapes(), 0);
        assert!(!restored.cancelled.load(Ordering::SeqCst));
        assert_eq!(runtime.block_on(row(&case)), active_row());

        let other = case.channel.get() + 5_000;
        let key = format!("{}:legacy-{other}", case.provider.as_str());
        let insert = sqlx::query("INSERT INTO sessions (session_key, provider, status, channel_id, instance_id) VALUES ($1, $2, 'turn_active', $3, $4)").bind(key).bind(case.provider.as_str());
        let insert = insert.bind(other.to_string()).bind(NODE_ID);
        runtime.block_on(insert.execute(pool)).unwrap();
        let (status, answer) = runtime.block_on(cancel_on(&case, other, false, true));
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer.get("outcome"), None, "{answer}");
        assert_eq!(
            remote_cancel_reply(&answer),
            crate::services::discord::commands::STOPPING_RESPONSE
        );
    });
}
