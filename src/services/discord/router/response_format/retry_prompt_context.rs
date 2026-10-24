use serde::{Deserialize, Serialize};

use crate::services::turn_orchestrator::Intervention;

pub(in crate::services::discord) mod store;

const PREFIX: &str = "\u{1e}agentdesk.retry-prompt.v1\n";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct Preludes {
    pub recovery: Option<String>,
    pub feedback: Option<String>,
    pub wip: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct RetryPromptContext {
    version: u8,
    legacy_rendered: bool,
    pub raw: Option<String>,
    pub selected: Preludes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_prompt: Option<String>,
}

impl RetryPromptContext {
    pub(in crate::services::discord) async fn consume_confirmed(
        &self,
        pool: Option<&sqlx::PgPool>,
        provider: &crate::services::provider::ProviderKind,
        channel: u64,
        turn_id: &str,
    ) -> Result<(), String> {
        store::consume_confirmed(pool, provider, channel, turn_id, self).await
    }

    pub(in crate::services::discord) fn raw(raw: Option<String>) -> Self {
        Self {
            version: 1,
            legacy_rendered: false,
            raw,
            selected: Preludes::default(),
            expected_prompt: None,
        }
    }

    pub(in crate::services::discord) fn for_routing(
        reply: Option<String>,
        queued: bool,
    ) -> Option<String> {
        let context = if queued {
            Self::restore(reply)
        } else {
            Self::raw(reply)
        };
        context.requeue()
    }

    pub(in crate::services::discord) fn restore(value: Option<String>) -> Self {
        Self::decode(value.as_deref()).unwrap_or(Self {
            legacy_rendered: true,
            ..Self::raw(value)
        })
    }

    pub(in crate::services::discord) fn decode(value: Option<&str>) -> Option<Self> {
        let context: Self = serde_json::from_str(value?.strip_prefix(PREFIX)?).ok()?;
        (context.version == 1).then_some(context)
    }

    pub(in crate::services::discord) fn encode(&self) -> Option<String> {
        serde_json::to_string(self)
            .ok()
            .map(|json| format!("{PREFIX}{json}"))
    }

    pub(in crate::services::discord) fn may_select(&self) -> bool {
        !self.legacy_rendered
    }

    pub(in crate::services::discord) fn render_with(
        &self,
        merge: impl Fn(Option<String>, Option<String>) -> Option<String>,
    ) -> Option<String> {
        if !self.may_select() {
            return self.raw.clone();
        }
        let mut reply = self.raw.clone();
        for block in [
            self.selected
                .recovery
                .as_deref()
                .and_then(super::format_session_retry_context),
            self.selected
                .feedback
                .as_deref()
                .and_then(super::format_voluntary_feedback_reminder),
            self.selected
                .wip
                .as_deref()
                .and_then(super::format_turn_end_wip_warning_injection),
        ] {
            reply = merge(reply, block);
        }
        reply
    }

    #[cfg(test)]
    fn render(&self) -> Option<String> {
        self.render_with(super::merge_reply_contexts)
    }

    pub(in crate::services::discord) fn requeue(&self) -> Option<String> {
        // Selection only borrows the channel stash until confirmed input; handback consumes nothing.
        Self {
            selected: Preludes::default(),
            expected_prompt: None,
            ..self.clone()
        }
        .encode()
    }
}

impl crate::services::discord::inflight::InflightTurnState {
    pub(in crate::services::discord) fn retry_prompt_snapshot(&self) -> Option<RetryPromptContext> {
        RetryPromptContext::decode(self.followup_reply_context.as_deref())
    }
}

impl Intervention {
    pub(crate) fn retry_prompt_context_is_empty(&self) -> bool {
        self.reply_context.is_none()
            || RetryPromptContext::decode(self.reply_context.as_deref())
                .is_some_and(|ctx| ctx.raw.is_none() && ctx.selected == Preludes::default())
    }
}

#[cfg(test)]
mod tests;
