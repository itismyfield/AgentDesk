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
    let (_nonce, started) = reconnect::launch(&fx, &launcher);
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
        let original = intake.front().await.1;
        let (entered, resume) =
            discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                .await
                .pause();
        let start = dispatch::start_idle_for_test(
            intake.shared.clone(),
            ChannelId::new(CHANNEL),
            intake.front().await,
        );
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
