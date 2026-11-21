//! The input the restart reader releases for a channel's own Retired Codex execution reaches the
//! owner dispatch and starts one new execution: one create, one prompt, one submit, then Bound.

use super::*;

const RETIRED: &str = "fedcba9876543210fedcba9876543210";

// The restart reader's test proves the input is released untouched; this proves its dispatch over
// that Retired row launches and submits exactly once.
#[test]
fn a_retired_codex_row_starts_one_new_execution_that_takes_its_input_once_and_binds_pg() {
    let fx = Fixture::admitted("retired-restart");
    let mut retired = crate::db::dispatched_sessions::hosted_execution::tests::record(
        &fx.owner,
        RETIRED,
        HostedState::Retired,
    );
    if let Some(expected) = retired.expected.as_mut() {
        expected.binding_provider = "codex".into();
    }
    fx.rt.block_on(async {
        sqlx::query("UPDATE sessions SET hosted_execution = $1 WHERE channel_id = $2")
            .bind(serde_json::to_value(&retired).unwrap())
            .bind(CHANNEL.to_string())
            .execute(&fx.pool)
            .await
            .unwrap();
    });
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let launched = Mutex::new(None);
    let row = HostedRecord::Known(retired);
    let (result, messages) = fx.turn(&row, &ports, || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2);
        *launched.lock().unwrap() = fx.answer(&nonce).map(|_| nonce);
    });
    assert_eq!(result, Ok(()));
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1, "one create");
    assert_eq!(fx.rig.sends(), prompt_sends(), "one prompt, one submit");
    let nonce = launched
        .into_inner()
        .unwrap()
        .expect("the new execution started");
    assert_ne!(nonce, RETIRED, "a new execution, not the retired one");
    match fx.record() {
        HostedRecord::Known(record) => {
            assert_eq!(record.state, HostedState::Bound);
            assert_eq!(record.execution_nonce, nonce);
        }
        other => panic!("{other:?}"),
    }
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, StreamMessage::Text { content } if content.trim() == "답")),
        "{messages:?}"
    );
}
