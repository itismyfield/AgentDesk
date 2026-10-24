//! A non-forced cancel through the production router on a live Codex Herdr turn this process
//! started takes its `/stop`: it completes nothing, and the turn settles on its own abort.
use super::super::super::*;
use super::*;

const AUTH: &str = "activation-http-cancel";

/// What the router's non-forced cancel answered, and the session row's status after it.
struct Cancelled {
    status: axum::http::StatusCode,
    answer: Value,
    row: String,
}

/// The router's non-forced cancel of the fixture's channel on a runtime serving `shared`.
fn http_cancel(fx: &Fixture, shared: &Arc<crate::services::discord::SharedData>) -> Cancelled {
    let channel = CHANNEL.to_string();
    fx.rt.block_on(async {
        let update =
            sqlx::query("UPDATE sessions SET status = 'turn_active' WHERE channel_id = $1")
                .bind(&channel);
        update.execute(&fx.pool).await.unwrap();
        let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
        registry.register("codex".into(), shared.clone()).await;
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some(AUTH.into());
        let pool = Some(fx.pool.clone());
        let app = crate::server::routes::cancel_route_test_app(config, pool, Some(registry), "h1");
        let (uri, bearer) = (format!("/turns/{channel}/cancel"), format!("Bearer {AUTH}"));
        let headers = [("authorization", bearer.as_str())];
        let post = crate::server::routes::cancel_route_test_post;
        let (status, answer) = post(&app, &uri, &headers, r#"{"force":false}"#).await;
        let read = sqlx::query_scalar("SELECT status FROM sessions WHERE channel_id = $1");
        let row = read.bind(&channel).fetch_one(&fx.pool).await.unwrap();
        Cancelled {
            status,
            answer,
            row,
        }
    })
}

/// A cold Codex turn cancelled over HTTP before its own start; its provider then aborts.
fn cancelled_turn() -> (
    Fixture,
    Cancelled,
    Live,
    Result<(), String>,
    Vec<StreamMessage>,
    PathBuf,
) {
    let fx = Fixture::admitted("activation-http");
    let _switch = ConfiguredSwitch::on();
    let launcher = Arc::new(Launcher::default());
    let shared = stop_runtime(&fx);
    let seen = Mutex::new(None);
    let (result, frames) = fx.turn(&HostedRecord::Legacy, &fx.ports(&launcher), || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        if !wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            return;
        }
        let token = fx.cancel.lock().unwrap().clone();
        mailbox_turn(&fx, &shared, token.clone());
        fx.rig.answer("pane.read", screen(WORKING));
        assert_eq!(HERDR_CANCEL_OVERRIDE.with(std::cell::Cell::get), None);
        let cancelled = http_cancel(&fx, &shared);
        let session = uuid::Uuid::new_v4().to_string();
        let path = fx.rollout(&session);
        assert_eq!(fx.session_start(&context_of(&nonce), &session, &path), 202);
        append(&path, &started_lines("t1", "답"));
        let escapes = settle_escapes(&fx, 1);
        let channel = poise::serenity_prelude::ChannelId::new(CHANNEL);
        let current = fx.rt.block_on(shared.mailbox(channel).snapshot());
        let state = token
            .herdr_interrupt_state()
            .expect("the live turn's own state");
        let live = Live {
            stop: None,
            intent: state.user_stop.load(Ordering::SeqCst),
            escapes,
            exit: seal_exit(&token, false, false),
            owns_mailbox: current
                .cancel_token
                .is_some_and(|current| Arc::ptr_eq(&current, &token)),
        };
        let aborted = json!({"type": "event_msg", "payload": {"type": "turn_aborted",
            "turn_id": "t1"}});
        append(&path, &[aborted]);
        *seen.lock().unwrap() = Some((cancelled, live, path));
    });
    let (cancelled, live, path) = seen
        .into_inner()
        .unwrap()
        .expect("the turn reached its start");
    (fx, cancelled, live, result, frames, path)
}

// The cancel answers a pending stop with no completion; the turn keeps its token and row, takes one
// Escape and ends on its own Aborted. Only the executor runs: the queued input is the relay e2e's.
#[test]
fn a_non_forced_http_cancel_settles_a_live_turn_on_its_own_abort_pg() {
    let (fx, cancelled, live, result, frames, path) = cancelled_turn();
    assert_eq!(
        cancelled.status,
        axum::http::StatusCode::OK,
        "{}",
        cancelled.answer
    );
    let answer = &cancelled.answer;
    let claims = json!({
        "outcome": answer["outcome"], "intent": answer["intent"], "delivery": answer["delivery"],
        "reason": answer["reason"], "settlement": answer["settlement"],
        "turn_status": answer["turn_status"], "turn_completed_at": answer.get("turn_completed_at"),
        "inflight_cleared": answer["inflight_cleared"], "dispatch_cancelled": answer.get("dispatch_cancelled"),
        "mailbox_foreground_free": answer["mailbox_foreground_free"],
    });
    let pending = json!({
        "outcome": "herdr", "intent": "recorded", "delivery": "not_sent", "reason": "pending",
        "settlement": "host_owned", "turn_status": "stopping", "turn_completed_at": null,
        "inflight_cleared": false, "dispatch_cancelled": null, "mailbox_foreground_free": false,
    });
    assert_eq!(claims, pending);
    assert_eq!(
        cancelled.row, "turn_active",
        "the cancel left the session row as it was"
    );
    let expected = Live {
        stop: None,
        intent: true,
        escapes: 1,
        exit: ExitDecision::Hold,
        owns_mailbox: true,
    };
    assert_eq!(live, expected);
    assert_eq!(result, Ok(()));
    assert_eq!(own_aborts(&frames), 1, "{frames:?}");
    let escape = json!({"pane_id": PANE, "keys": ["esc"]});
    fx.rig.answer("pane.read", screen(READY));
    let launcher = Arc::new(Launcher::default());
    let (next, messages) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        reply(&fx, &path, 5, "t2", "둘째")
    });
    assert_eq!(next, Ok(()));
    assert_eq!(texts(&messages), ["둘째"]);
    let sent = [prompt_sends(), vec![escape], prompt_sends()].concat();
    assert_eq!(
        fx.rig.sends(),
        sent,
        "one Escape, then a later turn's prompt once"
    );
}
