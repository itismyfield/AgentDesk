use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GoalCommandKind {
    NotGoal,
    ChainedStart,
    FreshStart,
    Lifecycle(GoalLifecycleCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GoalLifecycleCommand {
    Pause,
    Resume,
    Clear,
}

impl GoalLifecycleCommand {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Clear => "clear",
        }
    }
}

pub(super) const GOAL_LIFECYCLE_SUBCOMMANDS: &[(&str, GoalLifecycleCommand)] = &[
    ("pause", GoalLifecycleCommand::Pause),
    ("resume", GoalLifecycleCommand::Resume),
    ("clear", GoalLifecycleCommand::Clear),
];

pub(super) fn classify_codex_goal_command(text: &str) -> GoalCommandKind {
    let Some(first_line) = text.trim_start().lines().next() else {
        return GoalCommandKind::NotGoal;
    };
    let first_line = first_line.trim_end();
    let Some(rest) = first_line.strip_prefix("/goal") else {
        return GoalCommandKind::NotGoal;
    };
    if !rest.is_empty() && !rest.chars().next().is_some_and(char::is_whitespace) {
        return GoalCommandKind::NotGoal;
    }
    let args = rest.trim_start();
    if args.is_empty() {
        return GoalCommandKind::ChainedStart;
    }
    for (sub, command) in GOAL_LIFECYCLE_SUBCOMMANDS {
        let Some(after) = args.strip_prefix(sub) else {
            continue;
        };
        if after.is_empty() || after.chars().next().is_some_and(char::is_whitespace) {
            return GoalCommandKind::Lifecycle(*command);
        }
    }
    if let Some(after_fresh) = args.strip_prefix("--fresh") {
        if after_fresh.is_empty() || after_fresh.chars().next().is_some_and(char::is_whitespace) {
            return GoalCommandKind::FreshStart;
        }
    }
    GoalCommandKind::ChainedStart
}

pub(super) fn classify_codex_goal_command_for_provider(
    provider: &ProviderKind,
    text: &str,
    channel_codex_goals_setting: Option<bool>,
) -> GoalCommandKind {
    if matches!(provider, ProviderKind::Codex) && channel_codex_goals_setting.unwrap_or(true) {
        classify_codex_goal_command(text)
    } else {
        GoalCommandKind::NotGoal
    }
}

pub(super) fn rewrite_fresh_goal_prompt(text: &str) -> String {
    let trimmed = text.trim_start();
    let prefix_len = text.len() - trimmed.len();
    let leading = &text[..prefix_len];
    let Some(rest) = trimmed.strip_prefix("/goal") else {
        return text.to_string();
    };
    let after_goal = rest.trim_start_matches(|c: char| c == ' ' || c == '\t');
    let Some(after_fresh) = after_goal.strip_prefix("--fresh") else {
        return text.to_string();
    };
    let objective = after_fresh.trim_start_matches(|c: char| c == ' ' || c == '\t');
    if objective.is_empty() {
        format!("{}/goal", leading)
    } else {
        format!("{}/goal {}", leading, objective)
    }
}

pub(super) fn codex_goal_lifecycle_notice(
    command: GoalLifecycleCommand,
    active_turn: bool,
) -> &'static str {
    match command {
        GoalLifecycleCommand::Clear if active_turn => {
            "`/goal clear`는 현재 Codex 턴이 끝난 뒤 적용할 수 있습니다. 현재 턴을 중단하려면 `/stop`을 먼저 사용해 주세요."
        }
        GoalLifecycleCommand::Clear => {
            "`/goal clear` 적용 완료: Codex goal 세션을 비웠습니다. 다음 Codex 턴은 fresh session으로 시작합니다."
        }
        GoalLifecycleCommand::Pause => {
            "`/goal pause`는 아직 routine lifecycle과 연결되어 있지 않아 Codex TUI로 전달하지 않았습니다."
        }
        GoalLifecycleCommand::Resume => {
            "`/goal resume`은 아직 routine lifecycle과 연결되어 있지 않아 Codex TUI로 전달하지 않았습니다."
        }
    }
}

pub(super) fn codex_goal_lifecycle_reason_code(
    command: GoalLifecycleCommand,
    active_turn: bool,
) -> &'static str {
    match command {
        GoalLifecycleCommand::Clear if active_turn => "codex_goal_clear_active_turn",
        GoalLifecycleCommand::Clear => "codex_goal_clear",
        GoalLifecycleCommand::Pause => "codex_goal_pause_ignored",
        GoalLifecycleCommand::Resume => "codex_goal_resume_ignored",
    }
}

pub(super) async fn send_codex_goal_lifecycle_notice(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    command: GoalLifecycleCommand,
    active_turn: bool,
) {
    let notice = codex_goal_lifecycle_notice(command, active_turn);
    rate_limit_wait(shared, channel_id).await;
    if let Err(error) = channel_id.say(http, notice).await {
        tracing::warn!(
            channel_id = channel_id.get(),
            command = command.as_str(),
            "failed to send Codex goal lifecycle notice: {error}"
        );
        let target = format!("channel:{}", channel_id.get());
        let session_key =
            build_adk_session_key(shared, channel_id, &ProviderKind::Codex, None).await;
        crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
            shared.pg_pool.as_ref(),
            &target,
            session_key.as_deref(),
            codex_goal_lifecycle_reason_code(command, active_turn),
            notice,
        );
    }
}

pub(super) async fn consume_codex_goal_lifecycle_command(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    command: GoalLifecycleCommand,
    stale_session_id: Option<String>,
) {
    let active_turn = super::super::super::mailbox_has_active_turn(shared, channel_id).await;
    if matches!(command, GoalLifecycleCommand::Clear) && !active_turn {
        super::super::super::commands::reset_channel_provider_state(
            http,
            shared,
            provider,
            channel_id,
            "/goal clear",
            true,
            false,
            false,
        )
        .await;
        if let Some(session_id) = stale_session_id.as_deref() {
            let _ = super::super::super::internal_api::clear_stale_session_id(session_id).await;
        }
    }

    send_codex_goal_lifecycle_notice(http, shared, channel_id, command, active_turn).await;
}

pub(super) async fn record_fresh_session_context_boundary(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> anyhow::Result<()> {
    crate::db::session_transcripts::record_channel_clear_boundary(
        shared.pg_pool.as_ref(),
        &channel_id.get().to_string(),
    )
    .await
}

#[cfg(test)]
mod codex_goal_lifecycle_unit_tests {
    use super::*;

    #[test]
    fn lifecycle_subcommands_are_classified_precisely() {
        assert_eq!(
            classify_codex_goal_command("/goal clear"),
            GoalCommandKind::Lifecycle(GoalLifecycleCommand::Clear)
        );
        assert_eq!(
            classify_codex_goal_command("/goal pause"),
            GoalCommandKind::Lifecycle(GoalLifecycleCommand::Pause)
        );
        assert_eq!(
            classify_codex_goal_command("/goal resume"),
            GoalCommandKind::Lifecycle(GoalLifecycleCommand::Resume)
        );
    }

    #[test]
    fn lifecycle_subcommands_have_consumed_notices() {
        assert!(
            codex_goal_lifecycle_notice(GoalLifecycleCommand::Clear, false).contains("적용 완료")
        );
        assert!(
            codex_goal_lifecycle_notice(GoalLifecycleCommand::Clear, true)
                .contains("현재 Codex 턴")
        );
        assert!(
            codex_goal_lifecycle_notice(GoalLifecycleCommand::Pause, false)
                .contains("Codex TUI로 전달하지 않았습니다")
        );
        assert!(
            codex_goal_lifecycle_notice(GoalLifecycleCommand::Resume, false)
                .contains("Codex TUI로 전달하지 않았습니다")
        );
    }

    fn durable_boundary_call() -> String {
        format!("{}{}", "record_fresh_session_", "context_boundary(")
    }

    fn assert_goal_fresh_records_before_provider_clear(source: &str, branch_start: &str) {
        let branch = source
            .find(branch_start)
            .unwrap_or_else(|| panic!("missing goal-fresh branch: {branch_start}"));
        let boundary = source[branch..]
            .find(&durable_boundary_call())
            .map(|offset| branch + offset)
            .expect("goal-fresh branch must record a durable transcript boundary");
        let provider_clear = source[branch..]
            .find("clear_codex_goal_start_provider_session(")
            .map(|offset| branch + offset)
            .expect("goal-fresh branch must clear the provider session");

        assert!(
            boundary < provider_clear,
            "durable /goal fresh boundary must be recorded before provider state is cleared"
        );
    }

    #[test]
    fn goal_fresh_intake_and_headless_paths_record_durable_boundary_before_clear() {
        assert_goal_fresh_records_before_provider_clear(
            include_str!("intake_turn.rs"),
            "let force_fresh_provider_session = matches!(turn_goal_kind, GoalCommandKind::FreshStart);",
        );
        assert_goal_fresh_records_before_provider_clear(
            include_str!("headless_turn.rs"),
            "let goal_fresh = matches!(headless_goal_kind, GoalCommandKind::FreshStart);",
        );

        let helper_source = include_str!("goal_lifecycle.rs");
        let helper_start = format!(
            "{}{}",
            "pub(super) async fn record_fresh_session_", "context_boundary("
        );
        let helper = helper_source
            .find(&helper_start)
            .expect("fresh-session boundary helper exists");
        let db_boundary_call = format!(
            "{}{}",
            "crate::db::session_transcripts::record_channel_", "clear_boundary("
        );
        assert!(
            helper_source[helper..].contains(&db_boundary_call),
            "fresh-session helper must persist the durable channel boundary"
        );
    }
}

pub(super) async fn clear_codex_goal_start_provider_session(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    adk_session_key: Option<&str>,
    session_id: &mut Option<String>,
    memento_context_loaded: &mut bool,
    session_strategy_reason: &mut &'static str,
) {
    let session_id_to_clear = session_id.clone();
    {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
    }

    if let Some(key) = adk_session_key {
        super::super::super::adk_session::clear_provider_session_id(key, shared.api_port).await;
    }
    if let Some(ref stale_session_id) = session_id_to_clear {
        let _ = super::super::super::internal_api::clear_stale_session_id(stale_session_id).await;
    }

    *session_id = None;
    *memento_context_loaded = false;
    *session_strategy_reason = "codex_goal_start_fresh_session";
}
