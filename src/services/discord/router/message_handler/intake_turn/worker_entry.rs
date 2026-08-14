use super::*;

/// Per-message inputs of `handle_text_message` bundled into a single
/// owned struct. Phase 2-pre.3 of intake-node-routing: lets worker-side
/// callers (`execute_intake_turn_core`) accept a single deserialized
/// row from `intake_outbox` instead of a long positional parameter list.
///
/// Payload fields mirror the `intake_outbox` columns (see
/// migrations/postgres/0052_intake_node_routing.sql) and the per-message
/// parameters of the legacy `handle_text_message` signature; the row primary
/// key is carried separately as `intake_outbox_id`.
/// Adding a column to `intake_outbox` means adding a field here.
#[derive(Clone, Debug)]
pub(crate) struct IntakeRequest {
    /// Worker rows carry their claimed `intake_outbox` primary key; leader-local
    /// requests have no outbox row and carry `None`.
    pub intake_outbox_id: Option<i64>,
    pub channel_id: ChannelId,
    pub user_msg_id: MessageId,
    pub busy_followup_retry_user_msg_id: MessageId,
    pub request_owner: UserId,
    pub request_owner_name: String,
    pub user_text: String,
    pub reply_to_user_message: bool,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
    pub merge_consecutive: bool,
    pub reply_context: Option<String>,
    pub has_reply_boundary: bool,
    pub dm_hint: Option<bool>,
    pub turn_kind: TurnKind,
    pub preserve_on_cancel: bool,
}

/// Worker-callable entry point for executing an intake turn. Phase 2-pre.3
/// of intake-node-routing: this is the public surface a worker node will
/// invoke after claiming an `intake_outbox` row from its target queue. Pass
/// the runtime primitives the worker has (`Arc<Http>`, `Arc<SharedData>`,
/// bot token) plus the deserialized message payload; the function constructs
/// `IntakeDeps` with `cache: None` and `ctx_for_chained_dispatch: None`
/// (workers have no live gateway shard) and delegates to the existing
/// intake body.
///
/// Leader producers use `router::intake_dispatch`; a claimed worker bypasses
/// admission so it cannot recursively create another outbox row.
pub(crate) async fn execute_intake_turn_core(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    token: &str,
    request: IntakeRequest,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    super::handle_text_message(
        &IntakeDeps {
            http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared,
            token,
        },
        request.preserve_on_cancel,
        request,
        false,
        Vec::new(),
        // Worker dispatch has no in-process gate carry-forward; it re-resolves
        // the durable announcement row for its `user_msg_id` (#3905).
        None,
    )
    .await
}
