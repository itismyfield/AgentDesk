//! A2 owners admit the real O intake and preserve its suspended-source FIFO contract.

use super::{BeforeInput, scenario};
use crate::services::discord::commands::control::codex_clear_relaunch_tests::CanaryFixture;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::absent_launch_tests::auxiliary_tests::joint_owner_tests::{
    OwnerScope, install_positive,
};
use crate::services::tui_o::cutover::test_override;

pub(super) async fn install(fixture: &mut CanaryFixture) -> OwnerScope {
    install_positive(fixture, false).await
}

#[tokio::test(flavor = "current_thread")]
async fn actual_gateway_mixed_foreign_owners_deliver_fresh_fifo_pg() {
    if test_override::isolated_binding_case(concat!(
        module_path!(),
        "::actual_gateway_mixed_foreign_owners_deliver_fresh_fifo_pg"
    )) {
        scenario(true, BeforeInput::ForeignInventory).await;
    }
}
