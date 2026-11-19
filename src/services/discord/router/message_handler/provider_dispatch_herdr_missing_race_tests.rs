//! The actual early Busy leaf loses its exact pane before its first capture.
use super::*;
use crate::services::claude_tui::host_input::busy::BusyState;
use crate::services::discord::queue_io::input_owner::{effect, held_watch};

fn capture_race(active: bool, unknown: bool, successor: bool) {
    let fx = Fixture::new("busy-capture-race", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let original_runtime;
    let mut intake = Intake::new(&fx, &started);
    original_runtime = recording_runtime(&mut intake, &fx, false);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    append(
        &started.path,
        &[
            json!({"type":"user","uuid":uuid(),"sessionId":started.session,"message":{"role":"user","content":"working"}}),
            json!({"type":"assistant","uuid":uuid(),"apiBlockIndex":0,"sessionId":started.session,"message":{"id":"busy-race","role":"assistant","content":[{"type":"text","text":"working"}]}}),
        ],
    );
    let screen = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/tui_input/claude-2.1.293-idle.txt"
    ));
    fx.rig.answer(
        "pane.read",
        json!({"type":"pane_read","read":{
        "pane_id":PANE,"source":"recent_unwrapped","text":screen.replace("Baked", "Thinking"),
        "revision":2,"truncated":false}}),
    );
    let sends = fx.rig.sends().len();
    intake.runtime.block_on(async {
        let key = discord::adk_session::turn_session_key(
            &intake.shared,
            ChannelId::new(CHANNEL),
            &ProviderKind::Claude,
        )
        .await
        .unwrap();
        let host = crate::services::turn_host::for_turn(
            Some(&fx.pool),
            &ProviderKind::Claude,
            CHANNEL,
            Some(&key),
        )
        .await;
        assert_eq!(
            claude_handoff::observe_busy(CHANNEL, fx.logical(), &host, &intake.shared.token_hash)
                .state(),
            BusyState::Busy
        );
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        let front = intake.front().await;
        let original = front.1.clone();
        let counts = dispatch::OwnerTurn::probes(CHANNEL);
        let (entered, release) = effect::pause_claude_capture(CHANNEL);
        let start =
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
        let lose_pane = async {
            tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
            let now = dispatch::OwnerTurn::probes(CHANNEL);
            assert_eq!(
                (now.0 - counts.0, now.1 - counts.1),
                (1, 0),
                "the actual early Busy admission completed before the leaf capture"
            );
            if active {
                intake
                    .mailbox
                    .restore_active_turn(
                        Arc::new(CancelToken::new()),
                        UserId::new(7),
                        MessageId::new(2),
                    )
                    .await;
            }
            if successor {
                let mut record: Value =
                    sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
                        .bind(CHANNEL.to_string())
                        .fetch_one(&fx.pool)
                        .await
                        .unwrap();
                let nonce = "e".repeat(32);
                record["execution_nonce"] = nonce.clone().into();
                record["source_ref"]["execution_nonce"] = nonce.clone().into();
                record["expected"]["binding_nonce"] = nonce.into();
                record["location"]["pane_id"] = "%900".into();
                sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE channel_id=$2")
                    .bind(record)
                    .bind(CHANNEL.to_string())
                    .execute(&fx.pool)
                    .await
                    .unwrap();
            }
            fx.rig.show_panes(&[]);
            if unknown {
                fx.rig.answer("session.snapshot", json!({"type":"ok"}));
            }
            fx.rig.answer("pane.read", json!({"type":"ok"}));
            eprintln!("EARLY_BUSY_CAPTURE_GATE_PROVED active={active} unknown={unknown}");
            release.send(()).unwrap();
        };
        let _rest = test_rest::install(intake._api.http.clone());
        let (result, ()) = tokio::join!(tokio::time::timeout(LIMIT, start), lose_pane);
        let result = result.unwrap();
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.starts_with("NotSent(") && e.contains("PaneUnavailable")),
            "{result:?}"
        );
        assert_eq!(
            fx.rig.sends().len(),
            sends,
            "the failed capture wrote no input"
        );
        let drive = super::super::source_retry_drive(&intake);
        let (_, look) = held_watch::watch_observed(drive, original.message_id);
        let look = tokio::time::timeout(LIMIT, look).await.unwrap().unwrap();
        assert_eq!(
            look.verdict,
            if unknown {
                held_watch::Verdict::Keep
            } else if active || successor {
                held_watch::Verdict::End
            } else {
                held_watch::Verdict::Wake
            }
        );
        if active || unknown || successor {
            let state = intake
                .mailbox
                .v2_request(|slot| slot.as_ref().unwrap().json())
                .await
                .unwrap();
            assert_eq!(state["items"], json!([original]));
            assert_eq!(state["retained_inputs"], json!([]));
            assert_eq!(
                sqlx::query_scalar::<_, String>(
                    "SELECT hosted_execution->>'state' FROM sessions WHERE channel_id=$1"
                )
                .bind(CHANNEL.to_string())
                .fetch_one(&fx.pool)
                .await
                .unwrap(),
                "bound"
            );
            assert_eq!(intake.mailbox.has_active_turn().await, Ok(active));
            assert_eq!(
                intake
                    ._api
                    .take()
                    .iter()
                    .filter(|call| call.contains("이 입력을 폐기했습니다"))
                    .count(),
                0
            );
        } else {
            let state = tokio::time::timeout(LIMIT, async {
                loop {
                    let state = intake
                        .mailbox
                        .v2_request(|slot| slot.as_ref().unwrap().json())
                        .await
                        .unwrap();
                    if state["items"] == json!([]) {
                        break state;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(state["retained_inputs"], json!([]));
            assert_eq!(
                sqlx::query_scalar::<_, String>(
                    "SELECT hosted_execution->>'state' FROM sessions WHERE channel_id=$1"
                )
                .bind(CHANNEL.to_string())
                .fetch_one(&fx.pool)
                .await
                .unwrap(),
                "retired"
            );
            tokio::time::timeout(LIMIT, async {
                loop {
                    let results = dispatch::OwnerTurn::drive_results(CHANNEL);
                    if results.iter().any(|r| r.contains("settled"))
                        && intake.mailbox.has_active_turn().await == Ok(false)
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                intake
                    ._api
                    .take()
                    .iter()
                    .filter(|call| call.contains("이 입력을 폐기했습니다"))
                    .count(),
                1
            );
            assert!(!held_watch::has_watch(CHANNEL));
        }
    });
    drop(intake);
    drop(original_runtime);
}

#[test]
fn a_pane_lost_after_early_busy_capture_rejudges_the_idle_front_once_pg() {
    capture_race(false, false, false);
}
#[test]
fn a_pane_lost_after_early_busy_capture_preserves_an_active_turn_front_pg() {
    capture_race(true, false, false);
}
#[test]
fn an_unknown_snapshot_after_early_busy_capture_does_not_retire_or_discard_pg() {
    capture_race(false, true, false);
}

#[test]
fn a_successor_after_early_busy_capture_is_not_retired_or_discarded_by_the_old_probe_pg() {
    capture_race(false, false, true);
}
