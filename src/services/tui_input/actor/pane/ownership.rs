use std::process::Command;
use std::time::Duration;

use crate::services::discord::input_runtime::offer::Offer;
use crate::services::tui_input::bounded_tmux::{BoundedTmuxError, Pending, spawn_with_budget};
use crate::services::tui_o::shadow::ShadowProvider;

pub(super) fn spawn(
    offer: &Offer,
    provider: ShadowProvider,
    handle: &tokio::runtime::Handle,
    command: &mut Command,
    budget: Duration,
    enter: bool,
) -> Result<Pending, BoundedTmuxError> {
    let mutant = if enter {
        "pre_enter_epoch"
    } else {
        "pre_paste_epoch"
    };
    if super::super::super::transition::mutant(mutant) {
        return spawn_with_budget(handle, command, budget);
    }
    if provider == offer.provider
        && let Some(result) = offer.admit(|| spawn_with_budget(handle, command, budget))
    {
        return result;
    }
    Err(BoundedTmuxError::Spawn(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "input offer ownership epoch unavailable",
    )))
}
