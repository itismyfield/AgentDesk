pub(crate) mod busy_inject;
pub(crate) mod canary;
pub(crate) mod execution_end;
pub(crate) mod host_input;
pub mod input;
pub mod rollout_index;
pub mod rollout_tail;
pub mod session;
mod startup_dialog;
#[cfg(unix)]
pub(crate) use startup_dialog::is_startup_dialog_error;
pub(crate) mod tail_launch;
pub(crate) mod verified_hold;
pub(crate) mod verified_tail;
pub(crate) mod warm_followup;
