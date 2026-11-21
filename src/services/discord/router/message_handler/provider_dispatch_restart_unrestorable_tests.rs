use super::*;
use crate::services::claude_tui::readiness;
use crate::services::discord::queue_io::input_owner::held_watch::{self, Verdict};
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;

/// A proofless Busy hold no read can restore ends at the owner's due: nothing reaches the pane, the
/// original is refused without its body and one notice is posted; the hold never stays kept.
#[test]
fn an_unrestorable_busy_hold_ends_at_its_due_with_one_notice_and_no_key_pg() {
    let fx = Fixture::with_root(
        "restart-unrestorable-held-front",
        None,
        crate::config::TestRuntimeRootGuard::new(),
    );
    let launcher = Arc::new(Launcher::default());
    let (_nonce, started) = reconnect::launch_pinless(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = crate::services::discord::router::intake_gate::busy_inject::test_support::open_gate(
        CHANNEL,
    );
    let intake = Intake::with_expected_pin(&fx, &started, ExpectedPin::Registered);
    append(
        &started.path,
        &[
            json!({"type":"user","uuid":uuid(),"sessionId":started.session,
            "timestamp":chrono::Utc::now().to_rfc3339(),
            "message":{"role":"user","content":"work that never reports its end"}}),
        ],
    );
    readiness::tests::forget_session(fx.logical());
    assert!(readiness::observe_restored_reply(fx.logical(), Some(&started.path)).is_none());
    let heads = late_tests::write_on_enter(&fx, &intake, &started.session, &started.path, false);
    intake.runtime.block_on(async {
        let front = intake.front().await;
        let original = front.1.clone();
        let (entered, resume) =
            discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                .await
                .pause();
        let start =
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
        let holder = async {
            tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
            intake.install_holder(&fx, &started).await;
            resume.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(tokio::time::timeout(LIMIT, start), holder);
        assert!(
            result
                .expect("the busy admission holds")
                .is_err_and(|e| e.starts_with("NotSent("))
        );
        let (_, observed) =
            held_watch::watch_observed(source_retry_drive(&intake), original.message_id);
        let look = tokio::time::timeout(LIMIT, observed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            look.verdict,
            Verdict::Keep,
            "the hold is real before its due"
        );
        let settled = crate::test_support::wait::poll_within(
            async || {
                let snapshot = intake
                    .mailbox
                    .v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
                let snapshot = snapshot.await?;
                let told = snapshot["refused_fronts"][0]["notice"]["enqueued"] == true;
                told.then_some(snapshot)
                    .ok_or_else(|| "the held front is not settled yet".into())
            },
            LIMIT,
            "readiness due settlement",
        );
        let snapshot = settled.await.unwrap();
        let entry = &snapshot["refused_fronts"][0];
        assert_eq!(
            entry["input"]["identity"]["primary"], original.message_id,
            "{snapshot}"
        );
        assert!(
            entry["reason"]
                .as_str()
                .unwrap()
                .starts_with("readiness_deadline: ")
        );
        assert_eq!(snapshot["items"], json!([]), "{snapshot}");
        assert!(snapshot["head_injection"].is_null() && !snapshot.to_string().contains(INPUT));
        assert_eq!(fx.rig.sends().len(), before, "no key reached the pane");
        assert!(heads.lock().unwrap().is_empty());
        let pool = intake.shared.pg_pool.clone().expect("PG test pool");
        let notices: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_outbox WHERE reason_code = 'refused.readiness_unproven'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notices, 1);
    });
}

/// The same proofless Busy hold ends by delivery once the turn it waited behind is over and the
/// restored current reply plus an idle screen prove the pane ready: one send, no refusal.
#[test]
fn a_restored_reply_ends_the_same_busy_hold_with_one_delivery_and_no_refusal_pg() {
    let fx = Fixture::with_root(
        "restart-restored-held-front",
        None,
        crate::config::TestRuntimeRootGuard::new(),
    );
    let launcher = Arc::new(Launcher::default());
    let (_nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = crate::services::discord::router::intake_gate::busy_inject::test_support::open_gate(
        CHANNEL,
    );
    let intake = Intake::new(&fx, &started);
    let mut end = crate::services::tui_turn_state::idle_compaction_tests::end_turn();
    end["sessionId"] = json!(started.session);
    end["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
    append(&started.path, &[end]);
    let records = crate::services::tui_turn_state::idle_compaction_tests::tail();
    let records: Vec<_> = (records.into_iter())
        .map(|mut record| {
            record["sessionId"] = json!(started.session);
            record
        })
        .collect();
    append(&started.path, &records);
    readiness::tests::forget_session(fx.logical());
    assert!(readiness::observe(fx.logical(), Some(&started.path)).is_none());
    let idle = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_input/claude-2.1.293-idle.txt"
    ));
    let show = |text: &str| {
        fx.rig.answer(
            "pane.read",
            json!({"type":"pane_read","read":{
        "pane_id":PANE,"source":"recent_unwrapped","text":text,"revision":2,"truncated":false}}),
        )
    };
    show(&format!("✻ Thinking… (12s · esc to interrupt)\n{idle}"));
    let heads = late_tests::write_on_enter(&fx, &intake, &started.session, &started.path, false);
    intake.runtime.block_on(async {
        let front = intake.front().await;
        let original = front.1.clone();
        let (entered, resume) =
            discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                .await
                .pause();
        let start =
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
        let holder = async {
            tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
            intake.install_holder(&fx, &started).await;
            resume.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(tokio::time::timeout(LIMIT, start), holder);
        assert!(
            result
                .expect("the busy admission holds")
                .is_err_and(|e| e.starts_with("NotSent("))
        );
        let (_, observed) =
            held_watch::watch_observed(source_retry_drive(&intake), original.message_id);
        let look = tokio::time::timeout(LIMIT, observed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(look.verdict, Verdict::Keep);
        let (_, observed) =
            held_watch::watch_observed(source_retry_drive(&intake), original.message_id);
        let token = intake.mailbox.cancel_token().await.unwrap().unwrap();
        let persistence = discord::queue_persistence_context(
            &intake.shared,
            &ProviderKind::Claude,
            ChannelId::new(CHANNEL),
        );
        let _ = (intake.mailbox)
            .finish_turn_if_token(Some(token), persistence)
            .await;
        assert!(inflight::clear_inflight_state(
            &ProviderKind::Claude,
            CHANNEL
        ));
        show(idle);
        let look = tokio::time::timeout(LIMIT, observed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(look.verdict, Verdict::Wake);
        let snapshot = source_input_settled(&intake, &original, &heads).await;
        assert!(snapshot["refused_fronts"].is_null() && snapshot["refused_inputs"].is_null());
        let sends = fx.rig.sends();
        assert_eq!(sends.len() - before, 2, "the same original is sent once");
        assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
    });
}

/// After the intake starts: the restarted pane's completed reply, lost proof and its screen.
fn restored(fx: &Fixture, started: &Started, idle: bool) {
    let mut end = crate::services::tui_turn_state::idle_compaction_tests::end_turn();
    end["sessionId"] = json!(started.session);
    end["timestamp"] = json!(chrono::Utc::now().to_rfc3339());
    let records = crate::services::tui_turn_state::idle_compaction_tests::tail();
    let records = records.into_iter().map(|mut record| {
        record["sessionId"] = json!(started.session);
        record
    });
    append(
        &started.path,
        &std::iter::once(end).chain(records).collect::<Vec<_>>(),
    );
    let screen = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_input/claude-2.1.293-idle.txt"
    ));
    let busy = format!("✻ Thinking… (12s · esc to interrupt)\n{screen}");
    let text = if idle { screen.to_string() } else { busy };
    fx.rig.answer(
        "pane.read",
        json!({"type":"pane_read","read":{
        "pane_id":PANE,"source":"recent_unwrapped","text":text,"revision":2,"truncated":false}}),
    );
    readiness::tests::forget_session(fx.logical());
}

/// With no active turn, a restored current reply and an idle screen at the first read delegate to
/// the Idle start: the original is pasted and entered once, nothing is requeued or refused.
#[test]
fn a_restored_reply_with_no_active_turn_delivers_the_original_once_pg() {
    let fx = Fixture::with_root(
        "restart-no-holder-ready",
        None,
        crate::config::TestRuntimeRootGuard::new(),
    );
    let launcher = Arc::new(Launcher::default());
    let (_nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = crate::services::discord::router::intake_gate::busy_inject::test_support::open_gate(
        CHANNEL,
    );
    let intake = Intake::new(&fx, &started);
    restored(&fx, &started, true);
    assert!(readiness::observe(fx.logical(), Some(&started.path)).is_none());
    let heads = late_tests::write_on_enter(&fx, &intake, &started.session, &started.path, false);
    intake.runtime.block_on(async {
        let front = intake.front().await;
        let original = front.1.clone();
        let start =
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
        assert_eq!(tokio::time::timeout(LIMIT, start).await.unwrap(), Ok(()));
        let snapshot = source_input_settled(&intake, &original, &heads).await;
        assert!(snapshot["refused_fronts"].is_null() && snapshot["refused_inputs"].is_null());
        let sends = fx.rig.sends();
        assert_eq!(
            sends.len() - before,
            2,
            "the original is pasted and entered once"
        );
        assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
    });
}

/// The same pane still showing a running turn is not restored. With no active turn its absence
/// is unproven, so the original is held under the readiness due; an idle screen before that due
/// sends it once, with no notice.
#[test]
fn a_restored_reply_on_a_busy_screen_with_no_active_turn_is_delivered_once_when_idle_pg() {
    let fx = Fixture::with_root(
        "restart-no-holder-busy-idle",
        None,
        crate::config::TestRuntimeRootGuard::new(),
    );
    let launcher = Arc::new(Launcher::default());
    let (_nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = crate::services::discord::router::intake_gate::busy_inject::test_support::open_gate(
        CHANNEL,
    );
    let intake = Intake::new(&fx, &started);
    restored(&fx, &started, false);
    let heads = late_tests::write_on_enter(&fx, &intake, &started.session, &started.path, false);
    intake.runtime.block_on(async {
        let original = busy_screen_holds(&fx, &intake, before, &heads).await;
        show_idle_screen(&fx);
        let snapshot = source_input_settled(&intake, &original, &heads).await;
        assert!(snapshot["refused_fronts"].is_null() && snapshot["refused_inputs"].is_null());
        let sends = fx.rig.sends();
        assert_eq!(sends.len() - before, 2, "one paste and one Enter");
        assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
        assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
        assert_eq!(discard_notices(&intake), 0);
    });
}

/// The same held original whose screen never turns idle is settled unsent at the readiness due,
/// pulled forward here: one reason notice, no key and no body left in the queue.
#[test]
fn a_restored_reply_on_a_busy_screen_with_no_active_turn_is_settled_unsent_pg() {
    let fx = Fixture::with_root(
        "restart-no-holder-busy",
        None,
        crate::config::TestRuntimeRootGuard::new(),
    );
    let launcher = Arc::new(Launcher::default());
    let (_nonce, started) = reconnect::launch(&fx, &launcher);
    let before = fx.rig.sends().len();
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = crate::services::discord::router::intake_gate::busy_inject::test_support::open_gate(
        CHANNEL,
    );
    let intake = Intake::new(&fx, &started);
    restored(&fx, &started, false);
    let heads = late_tests::write_on_enter(&fx, &intake, &started.session, &started.path, false);
    intake.runtime.block_on(async {
        let original = busy_screen_holds(&fx, &intake, before, &heads).await;
        let due = intake
            .mailbox
            .v2_request(|slot| {
                let hold = slot.as_ref().ok_or("no owner")?.front_hold();
                let hold = hold.ok_or("the identity hold has no due")?;
                assert!(hold.identity_held_at_ms.is_some());
                assert_eq!(hold.due_at_ms - hold.started_at_ms, 10_000);
                Ok(hold.due_at_ms)
            })
            .await
            .unwrap();
        let ticked = intake
            .mailbox
            .v2_request(move |slot| slot.as_mut().ok_or("no owner")?.tick(due));
        ticked.await.unwrap();
        discord::queue_io::input_owner::drive::wake(source_retry_drive(&intake));
        let settled = crate::test_support::wait::poll_within(
            async || {
                let snapshot = intake
                    .mailbox
                    .v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
                let snapshot = snapshot.await?;
                let told = snapshot["refused_fronts"][0]["notice"]["enqueued"] == true;
                told.then_some(snapshot)
                    .ok_or_else(|| "the held front is not settled yet".into())
            },
            LIMIT,
            "identity hold due settlement",
        );
        let snapshot = settled.await.unwrap();
        let entry = &snapshot["refused_fronts"][0];
        assert_eq!(entry["input"]["identity"]["primary"], original.message_id);
        assert_eq!(
            entry["reason"], "readiness_deadline: busy_identity_unproven",
            "{snapshot}"
        );
        assert_eq!(snapshot["items"], json!([]), "{snapshot}");
        assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
        assert!(snapshot["head_injection"].is_null() && !snapshot.to_string().contains(INPUT));
        let pool = intake.shared.pg_pool.clone().expect("PG test pool");
        let notices: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_outbox WHERE reason_code = 'refused.readiness_unproven'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(notices, 1);
        assert_eq!(
            discard_notices(&intake),
            0,
            "no second, direct discard notice"
        );
        assert_eq!(fx.rig.sends().len(), before, "no key reached the pane");
        assert!(heads.lock().unwrap().is_empty());
    });
}

/// Starts the restored busy-screen front: it is held with no key, no notice and a watch.
async fn busy_screen_holds(
    fx: &Fixture,
    intake: &Intake,
    before: usize,
    heads: &Arc<Mutex<Vec<Value>>>,
) -> PendingQueueItem {
    let front = intake.front().await;
    let original = front.1.clone();
    let (entered, resume) =
        discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
            .await
            .pause();
    let start =
        dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
    let record = async {
        tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
        let guard = discord::shared_state::test_rest::install(intake._api.http.clone());
        resume.send(()).unwrap();
        guard
    };
    let (result, _record) = tokio::join!(tokio::time::timeout(LIMIT, start), record);
    let result = result.unwrap();
    assert!(result.is_err(), "the original is deferred: {result:?}");
    let state = intake
        .mailbox
        .v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
    let state = state.await.unwrap();
    assert_eq!(state["items"], json!([original]), "{state}");
    assert_eq!(state["retained_inputs"], json!([]), "{state}");
    assert!(held_watch::has_watch(CHANNEL));
    assert_eq!(discard_notices(intake), 0);
    assert_eq!(fx.rig.sends().len(), before, "no key reached the pane");
    assert!(heads.lock().unwrap().is_empty());
    original
}

fn show_idle_screen(fx: &Fixture) {
    let screen = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_input/claude-2.1.293-idle.txt"
    ));
    fx.rig.answer(
        "pane.read",
        json!({"type":"pane_read","read":{
        "pane_id":PANE,"source":"recent_unwrapped","text":screen,"revision":3,"truncated":false}}),
    );
}

fn discard_notices(intake: &Intake) -> usize {
    let notices = intake._api.take();
    (notices.iter())
        .filter(|call| call.contains("이 입력을 폐기했습니다"))
        .count()
}
