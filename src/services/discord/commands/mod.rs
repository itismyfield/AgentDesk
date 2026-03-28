mod config;
mod control;
mod diagnostics;
mod help;
mod meeting_cmd;
mod session;
mod skill;

pub(in crate::services::discord) use config::{
    MODEL_PICKER_CUSTOM_ID, MODEL_RESET_CUSTOM_ID, build_model_info_message,
    build_model_picker_components, build_model_picker_embed, build_model_status_message,
    is_clear_model_keyword, provider_supports_model_override, validate_model_input,
};
pub(super) use config::{cmd_adduser, cmd_allowed, cmd_allowedtools, cmd_model, cmd_removeuser};
pub(super) use control::{cmd_clear, cmd_down, cmd_shell, cmd_stop};
pub(in crate::services::discord) use diagnostics::{
    build_health_report, build_inflight_report, build_queue_report, build_status_report,
};
pub(super) use diagnostics::{
    cmd_debug, cmd_health, cmd_inflight, cmd_metrics, cmd_queue, cmd_status,
};
pub(super) use help::cmd_help;
pub(super) use meeting_cmd::cmd_meeting;
pub(super) use session::{cmd_pwd, cmd_start};
pub(super) use skill::cmd_cc;
