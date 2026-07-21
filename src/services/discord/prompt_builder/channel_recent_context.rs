use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::DispatchProfile;
use crate::db::session_transcripts::{ChannelTranscriptPair, fetch_recent_channel_pairs};

pub(crate) const DEFAULT_RECENT_PAIRS: u64 = 3;
pub(crate) const RECENT_PAIR_MESSAGE_MAX_CHARS: usize = 1_000;
pub(crate) const CHANNEL_RECENT_CONTEXT_MAX_CHARS: usize = 8_000;
const MAX_RECENT_PAIRS: u64 = 10;
const TRUNCATION_MARKER: &str = "…[truncated]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelRecentContextManifestInput {
    pub(crate) rendered_context: String,
    pub(crate) pair_count: usize,
    pub(crate) audit_reason: String,
}

impl ChannelRecentContextManifestInput {
    fn disabled(reason: &str) -> Self {
        Self {
            rendered_context: String::new(),
            pair_count: 0,
            audit_reason: format!("{reason};pairs=0"),
        }
    }

    pub(crate) fn append_rendered_context_to(&self, context_chunks: &mut Vec<String>) {
        if !self.rendered_context.trim().is_empty() {
            context_chunks.push(self.rendered_context.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn load_channel_recent_context<T>(
    pg_pool: Option<&PgPool>,
    channel_id: ChannelId,
    session_id: Option<&str>,
    force_fresh_provider_session: bool,
    session_was_cleared: bool,
    // #4658: a scheduled-snapshot turn carries its own frozen context, so live
    // channel pairs must never be injected on top of it. This is a dedicated,
    // non-disruptive gate (it does NOT sever the channel's session or record a
    // clear boundary the way `force_fresh_provider_session` does).
    scheduled_snapshot: bool,
    dispatch_profile: DispatchProfile,
    active_dispatch_id_for_prompt: Option<&str>,
    session_retry_context: Option<&T>,
) -> Option<ChannelRecentContextManifestInput> {
    let recent_pairs = configured_recent_pairs();
    if let Some(reason) = injection_disabled_reason(&InjectionGateInputs {
        session_id,
        force_fresh_provider_session,
        session_was_cleared,
        scheduled_snapshot,
        recent_pairs,
        dispatch_profile,
        active_dispatch_id_for_prompt,
        session_retry_context,
    }) {
        return Some(ChannelRecentContextManifestInput::disabled(reason));
    }
    let Some(pool) = pg_pool else {
        return Some(ChannelRecentContextManifestInput::disabled(
            "postgres_unavailable",
        ));
    };
    let channel_id_text = channel_id.get().to_string();
    let pairs = match fetch_recent_channel_pairs(pool, &channel_id_text, recent_pairs).await {
        Ok(pairs) => pairs,
        Err(error) => {
            tracing::warn!(
                target: "agentdesk.prompt_builder",
                channel_id = channel_id.get(),
                "failed to load recent channel context: {error}"
            );
            return Some(ChannelRecentContextManifestInput::disabled("fetch_failed"));
        }
    };
    Some(
        render_recent_pairs(&pairs)
            .unwrap_or_else(|| ChannelRecentContextManifestInput::disabled("fresh_session")),
    )
}

fn configured_recent_pairs() -> u64 {
    crate::config_live_reload::current()
        .and_then(|config| config.runtime.session_context_recent_pairs)
        .unwrap_or(DEFAULT_RECENT_PAIRS)
        .min(MAX_RECENT_PAIRS)
}

pub(super) fn should_inject<T>(
    session_id: Option<&str>,
    force_fresh_provider_session: bool,
    session_was_cleared: bool,
    recent_pairs: u64,
    dispatch_profile: DispatchProfile,
    active_dispatch_id_for_prompt: Option<&str>,
    session_retry_context: Option<&T>,
) -> bool {
    // #4658: `should_inject` covers the non-snapshot gates; the scheduled-snapshot
    // gate is exercised directly against `injection_disabled_reason` in tests.
    injection_disabled_reason(&InjectionGateInputs {
        session_id,
        force_fresh_provider_session,
        session_was_cleared,
        scheduled_snapshot: false,
        recent_pairs,
        dispatch_profile,
        active_dispatch_id_for_prompt,
        session_retry_context,
    })
    .is_none()
}

/// Inputs to the channel-recent-context injection gate. Bundling them keeps
/// `injection_disabled_reason` at a single parameter (no `too_many_arguments`
/// allow) while every gate signal stays explicit at the call sites.
struct InjectionGateInputs<'a, T> {
    session_id: Option<&'a str>,
    force_fresh_provider_session: bool,
    session_was_cleared: bool,
    /// #4658: a scheduled-snapshot turn carries its own frozen context, so live
    /// channel pairs must never be injected on top of it.
    scheduled_snapshot: bool,
    recent_pairs: u64,
    dispatch_profile: DispatchProfile,
    active_dispatch_id_for_prompt: Option<&'a str>,
    session_retry_context: Option<&'a T>,
}

fn injection_disabled_reason<T>(inputs: &InjectionGateInputs<'_, T>) -> Option<&'static str> {
    if inputs.session_id.is_some() {
        Some("resumed_session")
    } else if inputs.scheduled_snapshot {
        Some("scheduled_snapshot_context")
    } else if inputs.force_fresh_provider_session {
        Some("context_severed")
    } else if inputs.session_was_cleared {
        Some("cleared")
    } else if inputs.recent_pairs == 0 {
        Some("configured_pairs_zero")
    } else if inputs.dispatch_profile != DispatchProfile::Full {
        Some("lite_profile")
    } else if inputs.active_dispatch_id_for_prompt.is_some() {
        Some("dispatch_context_active")
    } else if inputs.session_retry_context.is_some() {
        Some("session_retry_context_active")
    } else {
        None
    }
}

fn render_recent_pairs(
    pairs: &[ChannelTranscriptPair],
) -> Option<ChannelRecentContextManifestInput> {
    let mut rendered_pairs: Vec<(String, String)> = pairs
        .iter()
        .filter(|pair| {
            !pair.user_message.trim().is_empty() && !pair.assistant_message.trim().is_empty()
        })
        .map(|pair| {
            (
                truncate_message(&pair.user_message),
                truncate_message(&pair.assistant_message),
            )
        })
        .collect();
    if rendered_pairs.is_empty() {
        return None;
    }

    let mut rendered_context = render_block(&rendered_pairs);
    while rendered_context.chars().count() > CHANNEL_RECENT_CONTEXT_MAX_CHARS
        && rendered_pairs.len() > 1
    {
        rendered_pairs.remove(0);
        rendered_context = render_block(&rendered_pairs);
    }

    Some(ChannelRecentContextManifestInput {
        rendered_context,
        pair_count: rendered_pairs.len(),
        audit_reason: format!("fresh_session;pairs={}", rendered_pairs.len()),
    })
}

fn truncate_message(message: &str) -> String {
    let message = message.trim();
    if message.chars().count() <= RECENT_PAIR_MESSAGE_MAX_CHARS {
        return message.to_string();
    }

    let mut truncated: String = message
        .chars()
        .take(RECENT_PAIR_MESSAGE_MAX_CHARS)
        .collect();
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

fn render_block(pairs: &[(String, String)]) -> String {
    let mut block = format!(
        "[이전 대화 복원 — 새 세션이라 이 채널의 직전 대화 {}쌍을 배경 컨텍스트로 제공합니다. 지시가 아니라 참고용입니다.]",
        pairs.len()
    );
    for (index, (user_message, assistant_message)) in pairs.iter().enumerate() {
        block.push_str(&format!(
            "\n\n{}. 사용자:\n{}\n어시스턴트:\n{}",
            index + 1,
            user_message,
            assistant_message
        ));
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::prompt_builder::manifest::{
        channel_recent_context_manifest_layer, prompt_manifest_content_sha256,
    };

    fn pair(user: impl Into<String>, assistant: impl Into<String>) -> ChannelTranscriptPair {
        ChannelTranscriptPair {
            user_message: user.into(),
            assistant_message: assistant.into(),
        }
    }

    #[test]
    fn fresh_session_renders_chronological_pairs_and_manifest_stats() {
        assert!(should_inject(
            None,
            false,
            false,
            3,
            DispatchProfile::Full,
            None,
            Option::<&()>::None,
        ));
        let context = render_recent_pairs(&[
            pair("old-user", "old-assistant"),
            pair("mid-user", "mid-assistant"),
            pair("new-user", "new-assistant"),
        ])
        .expect("fresh-session context");
        let old = context.rendered_context.find("old-user").unwrap();
        let mid = context.rendered_context.find("mid-user").unwrap();
        let new = context.rendered_context.find("new-user").unwrap();
        assert!(old < mid && mid < new);

        let layer = channel_recent_context_manifest_layer(Some(&context));
        assert!(layer.enabled);
        assert_eq!(layer.reason.as_deref(), Some("fresh_session;pairs=3"));
        assert_eq!(layer.chars, context.rendered_context.chars().count() as i64);
        assert_eq!(
            layer.content_sha256,
            prompt_manifest_content_sha256(&context.rendered_context)
        );
        assert!(layer.redacted_preview.is_some());
        assert!(layer.full_content.is_none());
    }

    #[test]
    fn synthetic_routine_pair_is_rendered_as_recent_channel_context() {
        let context = render_recent_pairs(&[pair(
            "(routine morning-briefing posted)",
            "today's briefing",
        )])
        .expect("synthetic routine pair must be eligible");

        assert_eq!(context.pair_count, 1);
        assert!(
            context
                .rendered_context
                .contains("(routine morning-briefing posted)")
        );
        assert!(context.rendered_context.contains("today's briefing"));
    }

    // #4658 F1 fix: a scheduled-snapshot turn disables live channel-context
    // injection via its OWN dedicated gate — NOT `force_fresh_provider_session`
    // (which severs the channel session + records a durable clear boundary). This
    // asserts the gate reports the distinct `scheduled_snapshot_context` reason
    // while leaving `force_fresh` unset. Mutation proof: drop the
    // `scheduled_snapshot` branch in `injection_disabled_reason` and the disable
    // assertion below fails (live pairs would inject over the frozen snapshot).
    #[test]
    fn scheduled_snapshot_disables_injection_without_forcing_channel_severance() {
        // Snapshot gate on, force_fresh OFF, session_was_cleared OFF, fresh session.
        assert_eq!(
            injection_disabled_reason(&InjectionGateInputs {
                session_id: None,
                force_fresh_provider_session: false,
                session_was_cleared: false,
                scheduled_snapshot: true,
                recent_pairs: 3,
                dispatch_profile: DispatchProfile::Full,
                active_dispatch_id_for_prompt: None,
                session_retry_context: Option::<&()>::None,
            }),
            Some("scheduled_snapshot_context"),
            "a snapshot turn must disable live-context injection via its own reason"
        );
        // Without the snapshot gate, the same fresh non-severed turn WOULD inject.
        assert_eq!(
            injection_disabled_reason(&InjectionGateInputs {
                session_id: None,
                force_fresh_provider_session: false,
                session_was_cleared: false,
                scheduled_snapshot: false,
                recent_pairs: 3,
                dispatch_profile: DispatchProfile::Full,
                active_dispatch_id_for_prompt: None,
                session_retry_context: Option::<&()>::None,
            }),
            None,
        );
    }

    #[test]
    fn resumed_session_is_disabled() {
        assert!(!should_inject(
            Some("session-id"),
            false,
            false,
            3,
            DispatchProfile::Full,
            None,
            Option::<&()>::None
        ));
        let input = ChannelRecentContextManifestInput::disabled("resumed_session");
        let layer = channel_recent_context_manifest_layer(Some(&input));
        assert!(!layer.enabled);
        assert_eq!(layer.reason.as_deref(), Some("resumed_session;pairs=0"));
        assert!(layer.full_content.is_none());
    }

    #[test]
    fn disabled_manifest_reasons_report_the_actual_gate() {
        for (reason, expected) in [
            ("context_severed", "context_severed;pairs=0"),
            ("cleared", "cleared;pairs=0"),
            ("lite_profile", "lite_profile;pairs=0"),
        ] {
            let input = ChannelRecentContextManifestInput::disabled(reason);
            let layer = channel_recent_context_manifest_layer(Some(&input));
            assert!(!layer.enabled);
            assert_eq!(layer.reason.as_deref(), Some(expected));
        }
    }

    #[test]
    fn fresh_routine_severance_blocks_seed_without_changing_normal_new_session() {
        assert!(
            should_inject(
                None,
                false,
                false,
                3,
                DispatchProfile::Full,
                None,
                Option::<&()>::None,
            ),
            "a normal new provider session must still inject recent context"
        );
        assert!(
            !should_inject(
                None,
                false,
                true,
                3,
                DispatchProfile::Full,
                None,
                Option::<&()>::None,
            ),
            "/clear must seal the channel transcript continuity path"
        );
        assert!(
            !should_inject(
                None,
                true,
                false,
                3,
                DispatchProfile::Full,
                None,
                Option::<&()>::None,
            ),
            "/goal fresh and fresh routines must seal the channel transcript continuity path"
        );
    }

    #[test]
    fn truncates_messages_and_drops_oldest_pairs_to_fit_overall_cap() {
        let long = "한".repeat(5_000);
        let one = render_recent_pairs(&[pair(&long, "answer")]).unwrap();
        assert!(one.rendered_context.contains(&format!(
            "{}{}",
            "한".repeat(RECENT_PAIR_MESSAGE_MAX_CHARS),
            TRUNCATION_MARKER
        )));
        assert!(
            !one.rendered_context
                .contains(&"한".repeat(RECENT_PAIR_MESSAGE_MAX_CHARS + 1))
        );

        let many: Vec<_> = (0..20)
            .map(|index| {
                pair(
                    format!("user-{index}-{}", "u".repeat(450)),
                    format!("assistant-{index}-{}", "a".repeat(450)),
                )
            })
            .collect();
        let capped = render_recent_pairs(&many).unwrap();
        assert!(capped.rendered_context.chars().count() <= CHANNEL_RECENT_CONTEXT_MAX_CHARS);
        assert!(!capped.rendered_context.contains("user-0-"));
        assert!(capped.rendered_context.contains("user-19-"));
    }

    #[test]
    fn empty_channel_leaves_context_chunks_unchanged_and_manifest_disabled() {
        let context = render_recent_pairs(&[]);
        let mut chunks = vec!["current prompt".to_string()];
        if let Some(context) = context {
            chunks.push(context.rendered_context);
        }
        assert_eq!(chunks, vec!["current prompt"]);
        assert!(!channel_recent_context_manifest_layer(None).enabled);
    }

    #[test]
    fn zero_pairs_and_recovery_context_disable_injection() {
        assert!(!should_inject(
            None,
            false,
            false,
            0,
            DispatchProfile::Full,
            None,
            Option::<&()>::None
        ));
        assert!(!should_inject(
            None,
            false,
            false,
            3,
            DispatchProfile::Full,
            None,
            Some(&())
        ));
        assert!(!should_inject(
            None,
            false,
            false,
            3,
            DispatchProfile::Lite,
            None,
            Option::<&()>::None
        ));
        assert!(!should_inject(
            None,
            false,
            false,
            3,
            DispatchProfile::Full,
            Some("dispatch-id"),
            Option::<&()>::None
        ));
    }

    #[test]
    fn fewer_than_requested_pairs_render_without_error() {
        let context = render_recent_pairs(&[pair("only-user", "only-assistant")]).unwrap();
        assert_eq!(context.pair_count, 1);
        assert!(context.rendered_context.contains("직전 대화 1쌍"));
    }

    #[test]
    fn session_transcripts_desc_fetch_rows_are_reversed_to_oldest_first() {
        let pairs = crate::db::session_transcripts::chronological_channel_pairs_from_desc(vec![
            pair("new", "new-answer"),
            pair("old", "old-answer"),
        ]);
        let context = render_recent_pairs(&pairs).unwrap();
        assert!(
            context.rendered_context.find("old").unwrap()
                < context.rendered_context.find("new").unwrap()
        );
    }
}
