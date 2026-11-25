//! A turn-mode Herdr channel whose create was proven unsent is not left waiting on presence.
use super::*;
use crate::services::herdr_launch::{HerdrLaunchError, retire_unsent_for_test as retire_unsent};

// The launch listed its execution, then retired it with no pane; presence reads no session and
// the input that waited on it is taken, as a routine waiting on that presence proceeds.
#[tokio::test(flavor = "current_thread")]
async fn presence_waiting_routine_proceeds_after_unsent_retire_pg() {
    let id = 71_340_190;
    let rig = Rig::new(id, true, false).await;
    rig.shared.core.lock().await.sessions.remove(&rig.channel);
    let name = ProviderKind::Claude.build_tmux_session_name(&format!("s1e-{id}"));
    let mut owner = None;
    seed_row_with(&rig, &name, Case::Pending, "unsent", |execution| {
        execution.location = None;
        execution.expected = None;
        owner = Some(execution.owner.clone());
    })
    .await;
    crate::services::tui_prompt_dedupe::install_herdr_execution(&name, "unsent");
    let pool = rig.shared.pg_pool.as_ref().unwrap();
    let unsent = HerdrLaunchError::NotSent("connect refused".into());
    let returned = retire_unsent(pool, &owner.unwrap(), "unsent", unsent.clone()).await;
    assert_eq!(returned, unsent);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(id);
    assert_eq!(settle_and_take(&rig, "unsent").await, ("no_session", true));
}
