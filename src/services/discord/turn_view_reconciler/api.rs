use super::*;

pub(in crate::services::discord) fn turn_view_owner_for_message(
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
) -> TurnViewOwner {
    TurnViewOwner::for_message(channel_id, message_id, generation)
}

pub(in crate::services::discord) async fn note_intake_turn_started(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_started(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_started_with_attempt(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> TurnViewStartRecord {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_started_with_attempt(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_queue_marker_added(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    emoji: char,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_queue_marker_added(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            emoji,
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_start_rolled_back_to_queued(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    start_attempt: TurnStartAttempt,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_start_rolled_back_to_queued(shared, target, owner, start_attempt, source)
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_completed(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_completed(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_failed(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_failed(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_cleared(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared_if_attempt_matches(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    start_attempt: TurnStartAttempt,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_cleared_if_attempt_matches(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            start_attempt,
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared_current_if_attempt_matches(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    start_attempt: Option<TurnStartAttempt>,
    source: &'static str,
) -> bool {
    let Some(start_attempt) = start_attempt else {
        return true;
    };
    note_intake_turn_cleared_if_attempt_matches(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        start_attempt,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_queue_marker_removed(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    emoji: char,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_queue_marker_removed(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeHttp(http.clone()),
            emoji,
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_started_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    source: &'static str,
) -> bool {
    note_intake_turn_started(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_started_current_with_attempt(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    source: &'static str,
) -> TurnViewStartRecord {
    note_intake_turn_started_with_attempt(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_queue_marker_added_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
    source: &'static str,
) -> bool {
    note_intake_queue_marker_added(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        emoji,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_start_rolled_back_to_queued_current(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    start_attempt: TurnStartAttempt,
    source: &'static str,
) -> bool {
    note_intake_start_rolled_back_to_queued(
        shared,
        channel_id,
        message_id,
        shared.restart.current_generation,
        start_attempt,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    source: &'static str,
) -> bool {
    note_intake_turn_cleared(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_queue_marker_removed_current(
    shared: &Arc<SharedData>,
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
    source: &'static str,
) -> bool {
    note_intake_queue_marker_removed(
        shared,
        http,
        channel_id,
        message_id,
        shared.restart.current_generation,
        emoji,
        source,
    )
    .await
}

async fn note_intake_turn_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    state: TurnViewState,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::intake_user_message(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_state(
            shared,
            target,
            owner,
            TurnViewIdentity::IntakeShared,
            state,
            source,
        )
        .await
}

pub(in crate::services::discord) async fn note_intake_turn_completed_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    note_intake_turn_via_shared(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Completed,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_failed_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    note_intake_turn_via_shared(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Failed,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_stopped_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    note_intake_turn_via_shared(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Stopped,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_intake_turn_cleared_via_shared(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    note_intake_turn_via_shared(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::None,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_tui_anchor_started(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::tui_direct_bot_anchor(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_started(shared, target, owner, TurnViewIdentity::ProviderBot, source)
        .await
}

pub(in crate::services::discord) async fn note_tui_anchor_completed(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> bool {
    let target = TurnViewTarget::tui_direct_bot_anchor(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_turn_completed(shared, target, owner, TurnViewIdentity::ProviderBot, source)
        .await
}

pub(in crate::services::discord) async fn note_tui_anchor_completed_delivery(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> TurnViewDelivery {
    note_tui_anchor_delivery(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Completed,
        source,
    )
    .await
}

pub(in crate::services::discord) async fn note_tui_anchor_failed_delivery(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    source: &'static str,
) -> TurnViewDelivery {
    note_tui_anchor_delivery(
        shared,
        channel_id,
        message_id,
        generation,
        TurnViewState::Failed,
        source,
    )
    .await
}

async fn note_tui_anchor_delivery(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    generation: u64,
    state: TurnViewState,
    source: &'static str,
) -> TurnViewDelivery {
    let target = TurnViewTarget::tui_direct_bot_anchor(channel_id, message_id);
    let owner = turn_view_owner_for_message(channel_id, message_id, generation);
    shared
        .turn_view_reconciler
        .note_state_delivery(
            shared,
            target,
            owner,
            TurnViewIdentity::ProviderBot,
            state,
            source,
        )
        .await
}
