mod config;
mod control;
mod diagnostics;
mod help;
mod meeting_cmd;
mod model_picker;
mod model_ui;
mod receipt;
mod session;
mod skill;

pub(in crate::services::discord) use super::model_catalog::{
    provider_supports_model_override, validate_model_input,
};
pub(in crate::services::discord) use config::{
    ModelPickerAction, build_model_picker_components_from_snapshot,
    build_model_picker_embed_from_snapshot, clear_model_picker_pending, current_working_dir,
    effective_model_snapshot, model_picker_pending_to_override, parse_model_picker_custom_id,
    resolve_model_for_turn, update_channel_model_override,
};
pub(super) use config::{cmd_adduser, cmd_allowall, cmd_allowed, cmd_allowedtools, cmd_removeuser};
pub(in crate::services::discord) use control::{
    clear_channel_session_state, reset_provider_session_if_pending,
};
pub(super) use control::{cmd_clear, cmd_down, cmd_shell, cmd_stop};
pub(in crate::services::discord) use diagnostics::{
    build_health_report, build_inflight_report, build_queue_report, build_status_report,
};
pub(super) use diagnostics::{
    cmd_debug, cmd_health, cmd_inflight, cmd_metrics, cmd_queue, cmd_status,
};
pub(super) use help::cmd_help;
pub(super) use meeting_cmd::cmd_meeting;
pub(super) use model_picker::cmd_model;
pub(super) use receipt::cmd_receipt;
pub(super) use session::{cmd_pwd, cmd_start};
pub(in crate::services::discord) use skill::build_provider_skill_prompt;
pub(super) use skill::cmd_cc;
