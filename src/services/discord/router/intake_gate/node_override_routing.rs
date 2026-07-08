use super::*;

pub(super) async fn try_route_intake_for_message(
    data: &Data,
    channel_id: serenity::ChannelId,
    user_msg_id: serenity::MessageId,
    has_attachments: bool,
    user_id: serenity::UserId,
    user_name: &str,
    text: &str,
    reply_context: Option<&str>,
    has_reply_boundary: bool,
    is_dm: bool,
    merge_consecutive: bool,
    turn_kind: super::super::message_handler::TurnKind,
) -> Option<crate::services::cluster::intake_router_hook::IntakeRouterDecision> {
    if has_attachments {
        tracing::debug!(
            channel_id = %channel_id,
            user_msg_id = %user_msg_id,
            "[intake_router] Discord attachments are node-local — running locally"
        );
        return None;
    }

    let pg_pool = data.shared.pg_pool.as_ref();
    let Some(pool) = pg_pool.as_ref() else {
        return None;
    };

    let mode = crate::services::cluster::intake_router_hook::effective_intake_routing_mode();
    let leader_instance_id =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let channel_id_str = channel_id.get().to_string();
    let user_msg_id_str = user_msg_id.get().to_string();
    let request_owner_id_str = user_id.get().to_string();
    let turn_kind_str = match turn_kind {
        super::super::message_handler::TurnKind::Foreground => "foreground",
        super::super::message_handler::TurnKind::BackgroundTrigger => "background_trigger",
    };
    let node_override =
        super::super::super::commands::channel_node_override(&data.shared, channel_id);
    let hook_ctx = crate::services::cluster::intake_router_hook::IntakeRouterContext {
        mode,
        leader_instance_id: &leader_instance_id,
        provider: data.provider.as_str(),
        channel_id: &channel_id_str,
        user_msg_id: &user_msg_id_str,
        request_owner_id: &request_owner_id_str,
        request_owner_name: Some(user_name),
        user_text: text,
        reply_context,
        has_reply_boundary,
        dm_hint: Some(is_dm),
        turn_kind: turn_kind_str,
        merge_consecutive,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        node_override_instance_id: node_override.as_deref(),
    };
    Some(crate::services::cluster::intake_router_hook::try_route_intake(pool, &hook_ctx).await)
}
