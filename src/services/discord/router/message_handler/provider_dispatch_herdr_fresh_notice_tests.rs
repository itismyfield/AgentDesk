//! A channel session whose transcript is gone, launched on Herdr.

use super::*;

/// The lifecycle notices the outbox holds for the channel's lost session `session`.
pub(super) fn notices(fx: &Fixture, session: &str) -> i64 {
    let query = sqlx::query_scalar(
        "SELECT count(*) FROM message_outbox
          WHERE target = $1 AND reason_code = 'lifecycle.session_resume_failed'
            AND session_key = $2",
    )
    .bind(format!("channel:{CHANNEL}"))
    .bind(format!("claude_resume_lost:{session}"));
    fx.rt.block_on(query.fetch_one(&fx.pool)).unwrap()
}

// A session whose transcript is gone starts fresh on Herdr with one channel notice in the outbox;
// its launch tried again after a restart posts no second one.
#[test]
fn a_herdr_resume_without_its_transcript_posts_one_notice_across_a_restart_pg() {
    let fx = Fixture::new("fresh-notice", None);
    let (session, path) = fx.seed_resumable(true);
    std::fs::remove_file(&path).unwrap();
    let launcher = Arc::new(Launcher::default());
    let refused = HerdrCreateOutcome::NotSent("connect refused".into());
    *launcher.create_fails.lock().unwrap() = Some(refused);
    let ports = fx.ports(&launcher);
    let (first, _) = fx.turn(&HostedRecord::Legacy, &ports, || {});
    assert!(first.unwrap_err().contains("NotSent"));
    assert_eq!(notices(&fx, &session), 1);

    crate::services::claude::fresh_notice::forget_for_tests(&session);
    let again = fx.ports(&launcher);
    let (second, _) = fx.turn(&fx.record(), &again, || {
        if let Some(started) = fx.start_provider(&launcher, true) {
            fx.answer(&started);
        }
    });
    assert_eq!(second, Ok(()));
    let script = fx.launch_script();
    assert!(!script.contains(&session), "{script}");
    assert!(script.contains("'--session-id'"), "{script}");
    assert_eq!(notices(&fx, &session), 1);
}
