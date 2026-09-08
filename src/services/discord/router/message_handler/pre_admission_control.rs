//! #5660 S3: Discord-side pre-admission control routing.
//!
//! Runs before the mailbox turn claim and the inflight record in
//! `intake_turn::handle_text_message`, so an input that completes locally never
//! opens a turn lifecycle. The `/goal` lifecycle block moved here unchanged; the
//! Codex control gate is the new part and calls the same classifier the terminal
//! wrapper calls, so the two origins cannot drift on what counts as a command.

use super::super::super::turn_view_reconciler::note_intake_turn_cleared_current as tv_clear_current;
use super::*;
use crate::services::tui_prompt_control::{
    CODEX_LOCAL_CONTROLS, CodexInputClass, classify_codex_input, raw_slash_invocation_parts,
};

/// Where one pre-admission input goes. Pure classification, no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PreAdmissionRoute {
    Continue(GoalCommandKind),
    GoalLifecycle(GoalLifecycleCommand),
    LocalControl { name: String, args: String },
    RejectControl { raw_name: String },
}

/// What [`resolve`] did with the input, as seen by the caller.
pub(super) enum PreAdmission {
    HandledLocally,
    Continue(GoalCommandKind),
}

/// The routing table; the first matching branch wins and the order is the
/// contract. `has_preloaded_uploads` describes the local `pending_uploads`
/// vector at the gate, which after `[R1]` holds exactly the uploads this
/// invocation was handed (live intake attachments, or a queued Intervention's
/// payload) — never the channel session's, which is taken below this gate.
pub(super) fn route(
    provider: &ProviderKind,
    channel_codex_goals_setting: Option<bool>,
    dispatch_reset: (bool, bool),
    has_preloaded_uploads: bool,
    user_text: &str,
) -> PreAdmissionRoute {
    // 1. A reset/recreate dispatch is never classified as a control.
    if dispatch_reset.0 || dispatch_reset.1 {
        return PreAdmissionRoute::Continue(GoalCommandKind::NotGoal);
    }
    // 2/3. `/goal` lifecycle completes locally; `/goal <objective>` opens a turn.
    match classify_codex_goal_command_for_provider(provider, user_text, channel_codex_goals_setting)
    {
        GoalCommandKind::Lifecycle(command) => return PreAdmissionRoute::GoalLifecycle(command),
        kind @ (GoalCommandKind::ChainedStart | GoalCommandKind::FreshStart) => {
            return PreAdmissionRoute::Continue(kind);
        }
        GoalCommandKind::NotGoal => {}
    }
    // 4. goals-disabled carve-out. `classify_codex_goal_command` matches `/goal`
    //    case-sensitively and `raw_slash_invocation_parts` keeps raw case, so
    //    this branch lowercases itself; otherwise `/GOAL clear` would fall
    //    through and be refused as an unsupported command.
    if raw_slash_invocation_parts(user_text)
        .is_some_and(|(raw_name, _)| raw_name.to_ascii_lowercase() == "/goal")
    {
        return PreAdmissionRoute::Continue(GoalCommandKind::NotGoal);
    }
    // 4.5 (#5660 P1-1). Input that already carries uploads must not take the
    //     local-completion path this gate introduces: the caller returns before
    //     the prompt is assembled, so the vector would be dropped instead of
    //     reaching the upload chunk. Deliberately BELOW branch 2 — above it,
    //     `/goal clear` plus an attachment would skip the lifecycle side effects.
    if has_preloaded_uploads {
        return PreAdmissionRoute::Continue(GoalCommandKind::NotGoal);
    }
    // 5/6. Codex control registry, shared verbatim with the wrapper; else continue.
    if !matches!(provider, ProviderKind::Codex) {
        return PreAdmissionRoute::Continue(GoalCommandKind::NotGoal);
    }
    match classify_codex_input(user_text) {
        CodexInputClass::LocalControl { name, args } => {
            PreAdmissionRoute::LocalControl { name, args }
        }
        CodexInputClass::UnsupportedControl { raw_name } => {
            PreAdmissionRoute::RejectControl { raw_name }
        }
        CodexInputClass::ProviderPrompt => PreAdmissionRoute::Continue(GoalCommandKind::NotGoal),
    }
}

/// Sound over-approximation of "this text can finish without a provider turn".
/// Lemma L: for every `(provider, goals, dispatch_reset, has_preloaded)` whose
/// [`route`] result is not `Continue(_)`, this predicate is true. It takes no
/// provider on purpose — `runtime_transition` calls it before the final provider
/// is resolved — so it over-includes. Over-inclusion leaves uploads in the
/// channel; under-inclusion would hand them to a record that drops them.
pub(super) fn may_complete_locally(user_text: &str) -> bool {
    match classify_codex_goal_command(user_text) {
        GoalCommandKind::Lifecycle(_) => true,
        GoalCommandKind::ChainedStart | GoalCommandKind::FreshStart => false,
        GoalCommandKind::NotGoal => matches!(
            classify_codex_input(user_text),
            CodexInputClass::LocalControl { .. } | CodexInputClass::UnsupportedControl { .. }
        ),
    }
}

/// `[R2]`: takes the channel's turn input state once the turn is committed —
/// the block that used to sit at the top of `handle_text_message`, moved below
/// the gate so a locally completed input consumes nothing. Must be called with
/// the ORIGINAL channel id, never a dispatch thread it was redirected to.
pub(super) async fn take_channel_input_state(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> (Vec<String>, bool) {
    let mut data = shared.core.lock().await;
    data.sessions
        .get_mut(&channel_id)
        .map(|s| {
            let was_cleared = s.cleared;
            s.cleared = false;
            (std::mem::take(&mut s.pending_uploads), was_cleared)
        })
        .unwrap_or_default()
}

/// `[R3']`: uploads-only take for the busy-transition handoff. `cleared` has no
/// `Intervention` field, so the session is the only place a later turn can read.
pub(super) async fn take_channel_input_state_uploads_only(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
) -> Vec<String> {
    let mut data = shared.core.lock().await;
    data.sessions
        .get_mut(&channel_id)
        .map(|s| std::mem::take(&mut s.pending_uploads))
        .unwrap_or_default()
}

/// Reason code and Discord text for a control that produced no provider turn.
/// `/model` reports and changes nothing, matching the wrapper (#5660); a
/// command-shaped token outside the registry is refused by its raw spelling.
fn codex_control_notice(routed: &PreAdmissionRoute) -> Option<(&'static str, String)> {
    match routed {
        PreAdmissionRoute::LocalControl { name, args } => {
            let mut notice = format!(
                "`{name}` 은 로컬에서 끝나는 Codex 컨트롤이라 provider 턴을 만들지 않았습니다."
            );
            if !args.is_empty() {
                notice.push_str(&format!(
                    "\n모델 변경은 지원하지 않습니다(읽기 전용). 요청한 값 `{args}` 은 적용되지 않았습니다."
                ));
            }
            Some(("codex_local_control", notice))
        }
        PreAdmissionRoute::RejectControl { raw_name } => Some((
            "codex_unsupported_control",
            format!(
                "지원하지 않는 명령입니다: `{raw_name}`\n지원 명령: {}\n경로를 보내려던 것이라면 하위 경로를 붙이거나(`/data/x`) 문장 앞에 단어를 두세요.",
                CODEX_LOCAL_CONTROLS.join(", ")
            ),
        )),
        _ => None,
    }
}

async fn send_pre_admission_notice(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    reason_code: &'static str,
    notice: &str,
) {
    rate_limit_wait(shared, channel_id).await;
    if let Err(error) = channel_id.say(http, notice).await {
        tracing::warn!(
            channel_id = channel_id.get(),
            reason_code,
            "failed to send Codex pre-admission control notice: {error}"
        );
        let target = format!("channel:{}", channel_id.get());
        let session_key =
            build_adk_session_key(shared, channel_id, &ProviderKind::Codex, None).await;
        crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
            shared.pg_pool.as_ref(),
            &target,
            session_key.as_deref(),
            reason_code,
            notice,
        );
    }
}

/// Executes [`route`]'s decision; `HandledLocally` means the caller returns.
pub(super) async fn resolve(
    runtime: (
        &Arc<serenity::http::Http>,
        &Arc<SharedData>,
        &ProviderKind,
        bool,
    ),
    channels: (ChannelId, ChannelId, MessageId),
    input: (&str, Option<&str>),
    dispatch_reset: (bool, bool),
    session_id: Option<String>,
) -> PreAdmission {
    let (http, shared, provider, has_preloaded_uploads) = runtime;
    let (channel_id, fast_mode_channel_id, user_msg_id) = channels;
    let (user_text, dispatch_id_for_thread) = input;
    // Branch 1 ignores this setting, so a reset dispatch keeps skipping the
    // lookup exactly as it did before the gate was extracted.
    let channel_codex_goals_setting = if dispatch_reset.0 || dispatch_reset.1 {
        None
    } else {
        super::super::super::commands::channel_codex_goals_setting(shared, fast_mode_channel_id)
            .await
    };
    match route(
        provider,
        channel_codex_goals_setting,
        dispatch_reset,
        has_preloaded_uploads,
        user_text,
    ) {
        PreAdmissionRoute::Continue(kind) => PreAdmission::Continue(kind),
        PreAdmissionRoute::GoalLifecycle(command) => {
            if should_add_turn_pending_reaction(dispatch_id_for_thread)
                && !super::super::super::voice_barge_in::is_synthetic_voice_message_id(user_msg_id)
            {
                tv_clear_current(shared, http, channel_id, user_msg_id, "intake_goal").await;
            }
            consume_codex_goal_lifecycle_command(
                http, shared, provider, channel_id, command, session_id,
            )
            .await;
            PreAdmission::HandledLocally
        }
        control => {
            if let Some((reason, notice)) = codex_control_notice(&control) {
                send_pre_admission_notice(http, shared, channel_id, reason, &notice).await;
            }
            PreAdmission::HandledLocally
        }
    }
}

#[cfg(test)]
mod pre_admission_control_tests {
    use super::*;

    const NO_RESET: (bool, bool) = (false, false);

    fn cont() -> PreAdmissionRoute {
        PreAdmissionRoute::Continue(GoalCommandKind::NotGoal)
    }

    fn codex(text: &str) -> PreAdmissionRoute {
        route(&ProviderKind::Codex, Some(true), NO_RESET, false, text)
    }

    /// (f1) S3-1..S3-3, S3-6..S3-10, S3-15, S3-30..S3-32: the #5660 P1-1 guard
    /// sits below the lifecycle branch and above the control registry, so
    /// `/goal clear` keeps its side effects while an attachment-carrying
    /// `/model` or unsupported control falls back to a provider turn.
    #[test]
    fn route_branch_table_is_pinned() {
        let chained = PreAdmissionRoute::Continue(GoalCommandKind::ChainedStart);
        let fresh = PreAdmissionRoute::Continue(GoalCommandKind::FreshStart);
        let clear = PreAdmissionRoute::GoalLifecycle(GoalLifecycleCommand::Clear);
        let model = |args: &str| PreAdmissionRoute::LocalControl {
            name: "/model".to_string(),
            args: args.to_string(),
        };
        let reject = |raw: &str| PreAdmissionRoute::RejectControl {
            raw_name: raw.to_string(),
        };
        let carried = |text: &str| route(&ProviderKind::Codex, Some(true), NO_RESET, true, text);
        assert_eq!(codex("/goal 작업"), chained);
        assert_eq!(codex("/goal --fresh 작업"), fresh);
        assert_eq!(codex("/goal clear"), clear);
        assert_eq!(codex("/model"), model(""));
        assert_eq!(codex("/model gpt-5.6-codex"), model("gpt-5.6-codex"));
        assert_eq!(codex("/tmp 용량을 설명해줘"), cont());
        assert_eq!(codex("/model\n실제 요청"), cont());
        assert_eq!(codex("/frobnicate"), reject("/frobnicate"));
        assert_eq!(carried("/model"), cont());
        assert_eq!(carried("/frobnicate"), cont());
        assert_eq!(carried("/goal clear"), clear);
    }

    /// (f1) S3-4, S3-5, S3-11..S3-14, S3-33: `/goal`-prefixed input survives
    /// whatever the goals setting (branch 4 normalises case, branch 2 does not),
    /// and neither Claude channels nor reset dispatches reach the registry.
    #[test]
    fn carve_outs_keep_input_out_of_the_control_registry() {
        let goals_off = |text| route(&ProviderKind::Codex, Some(false), NO_RESET, false, text);
        let claude = |text| route(&ProviderKind::Claude, Some(true), NO_RESET, false, text);
        let reset = |r, text| route(&ProviderKind::Codex, Some(true), r, false, text);
        assert_eq!(goals_off("/goal --fresh 작업"), cont());
        assert_eq!(goals_off("/goal clear"), cont());
        assert_eq!(goals_off("/GOAL clear"), cont());
        assert_eq!(codex("/GOAL clear"), cont());
        assert_eq!(claude("/model"), cont());
        assert_eq!(claude("/frobnicate"), cont());
        assert_eq!(reset((true, false), "/model"), cont());
        assert_eq!(reset((false, true), "/goal clear"), cont());
    }

    /// (f2b) S3-26': lemma L holds over the whole matrix and the strengthened
    /// attachment postcondition L' holds for the branches S3 adds. S3-27: the
    /// predicate must not widen into inputs that open a turn.
    #[test]
    fn every_non_continue_route_is_locally_completable() {
        let inputs = [
            "/goal 작업",
            "/goal --fresh 작업",
            "/goal clear",
            "/goal pause",
            "/GOAL clear",
            "/model",
            "/model gpt-5.6-codex",
            "/frobnicate",
            "/tmp 용량을 설명해줘",
            "/model\n실제 요청",
            "설명해줘",
        ];
        for provider in [ProviderKind::Codex, ProviderKind::Claude] {
            for goals in [Some(true), Some(false), None] {
                for reset in [NO_RESET, (true, false), (false, true)] {
                    for has_preloaded in [false, true] {
                        for text in inputs {
                            let routed = route(&provider, goals, reset, has_preloaded, text);
                            if matches!(routed, PreAdmissionRoute::Continue(_)) {
                                continue;
                            }
                            assert!(may_complete_locally(text), "lemma L broken by {text:?}");
                            let is_control = matches!(
                                routed,
                                PreAdmissionRoute::LocalControl { .. }
                                    | PreAdmissionRoute::RejectControl { .. }
                            );
                            assert!(!is_control || !has_preloaded, "L' broken by {text:?}");
                        }
                    }
                }
            }
        }
        for text in [
            "/goal 작업",
            "/goal --fresh 작업",
            "/tmp 용량",
            "/model\n요청",
        ] {
            assert!(!may_complete_locally(text), "{text:?}");
        }
    }

    fn session_with(uploads: &[&str], cleared: bool) -> DiscordSession {
        DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: None,
            history: Vec::new(),
            pending_uploads: uploads.iter().map(|u| (*u).to_string()).collect(),
            cleared,
            remote_profile_name: None,
            channel_id: None,
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        }
    }

    async fn fixture(session: Option<DiscordSession>) -> (Arc<SharedData>, ChannelId) {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(5_660_003);
        if let Some(session) = session {
            shared.core.lock().await.sessions.insert(channel, session);
        }
        (shared, channel)
    }

    /// (f2) S3-16..S3-18: the deferred take reproduces the block it replaced,
    /// including the absent-entry case, which must not create a ghost session.
    #[tokio::test]
    async fn deferred_take_reproduces_the_block_it_replaced() {
        let (shared, channel) = fixture(Some(session_with(&["U1", "U2"], false))).await;
        let taken = take_channel_input_state(&shared, channel).await;
        assert_eq!(taken, (vec!["U1".to_string(), "U2".to_string()], false));
        assert!(
            shared.core.lock().await.sessions[&channel]
                .pending_uploads
                .is_empty()
        );
        let (shared, channel) = fixture(Some(session_with(&[], true))).await;
        assert_eq!(
            take_channel_input_state(&shared, channel).await,
            (vec![], true)
        );
        assert!(!shared.core.lock().await.sessions[&channel].cleared);
        let (shared, channel) = fixture(None).await;
        assert_eq!(
            take_channel_input_state(&shared, channel).await,
            (vec![], false)
        );
        assert!(shared.core.lock().await.sessions.is_empty());
    }

    /// (f2) S3-19b: the handoff take must leave `cleared` alone — the queue
    /// record has no field for it, so the channel is its only home.
    #[tokio::test]
    async fn uploads_only_take_leaves_cleared_in_the_session() {
        let (shared, channel) = fixture(Some(session_with(&["U"], true))).await;
        let taken = take_channel_input_state_uploads_only(&shared, channel).await;
        assert_eq!(taken, vec!["U".to_string()]);
        let data = shared.core.lock().await;
        assert!(data.sessions[&channel].pending_uploads.is_empty());
        assert!(data.sessions[&channel].cleared);
    }

    fn offset(source: &str, token: &str) -> usize {
        source
            .find(token)
            .unwrap_or_else(|| panic!("missing token: {token}"))
    }

    /// (f3) S3-20a/20b/20g/M12: gate above the deferred take, no second session
    /// take, attachment flag read before it, and the prepend order kept.
    #[test]
    fn intake_turn_keeps_the_gate_above_the_deferred_take() {
        let src = include_str!("intake_turn.rs");
        let flag = "(http, shared, &provider, !pending_uploads.is_empty())";
        let take = "take_channel_input_state(";
        assert!(offset(src, "pre_admission_control::resolve(") < offset(src, take));
        assert!(offset(src, flag) < offset(src, take));
        assert_eq!(src.matches(flag).count(), 1);
        assert_eq!(src.matches("mem::take(&mut s.pending_uploads").count(), 0);
        assert_eq!(src.matches("splice(0..0, taken_uploads)").count(), 1);
    }

    /// (f3) S3-20c'/20d: the conditional handoff take lives inside the busy arm
    /// only, ahead of the enqueue, and its predicate cannot be dropped.
    #[test]
    fn runtime_transition_takes_uploads_only_inside_the_busy_arm() {
        let src = include_str!("intake_turn/runtime_transition.rs");
        let take = "take_channel_input_state_uploads_only(";
        let busy_arm = offset(src, "Err(_) => {");
        assert!(offset(src, "Ok(transition) => Ok(Some(transition))") < busy_arm);
        assert!(busy_arm < offset(src, "may_complete_locally("));
        assert!(offset(src, "may_complete_locally(") < offset(src, take));
        assert!(offset(src, take) < offset(src, "race_loss::handle_race_loss_enqueue("));
        assert_eq!(src.matches(take).count(), 1);
        assert_eq!(src.matches("may_complete_locally(").count(), 1);
    }

    /// (f3) S3-20e/20f: the guard stays between the lifecycle branch and the
    /// control registry, and this module never writes session uploads back (O4).
    #[test]
    fn guard_position_and_one_way_upload_ownership_are_pinned() {
        let src = include_str!("pre_admission_control.rs");
        let guard = format!("{}{}", "if has_preloaded_", "uploads {");
        let lifecycle = format!("{}{}", "return PreAdmissionRoute::", "GoalLifecycle(");
        let classify = format!("{}{}", "match classify_", "codex_input(user_text)");
        assert!(offset(src, &lifecycle) < offset(src, &guard));
        assert!(offset(src, &guard) < offset(src, &classify));
        assert_eq!(src.matches(&guard).count(), 1);
        for tail in ["push", "extend"] {
            let write = format!("{}{}", ".pending_uploads.", tail);
            assert_eq!(src.matches(&write).count(), 0, "{write}");
        }
    }
}
