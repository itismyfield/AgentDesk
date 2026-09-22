//! Shared ordering for user context; attachment lifetime remains with the provider invocation.
use super::*;
use crate::services::discord::prompt_builder::ChannelRecentContextManifestInput;

pub(super) struct TurnContext<'a> {
    pub provider: &'a ProviderKind,
    pub session_id: Option<&'a str>,
    pub uploads: &'a [String],
    pub trigger: Option<String>,
    pub reply: Option<&'a str>,
    pub recent: Option<&'a ChannelRecentContextManifestInput>,
    pub knowledge: Option<&'a str>,
    pub author_name: &'a str,
    pub author_id: UserId,
    pub user_input: String,
}

impl TurnContext<'_> {
    pub(super) fn build(self) -> String {
        let mut chunks = Vec::new();
        if !self.uploads.is_empty() {
            chunks.push(self.uploads.join("\n"));
        }
        if let Some(trigger) = self.trigger {
            chunks.push(trigger);
        }
        if let Some(reply) = self.reply {
            chunks.push(reply.to_string());
        }
        if let Some(recent) = self.recent {
            recent.append_rendered_context_to(&mut chunks);
        }
        if let Some(knowledge) = self.knowledge {
            chunks.push(knowledge.to_string());
        }
        chunks.push(wrap_user_prompt_with_author(
            self.author_name,
            self.author_id,
            self.user_input,
        ));
        crate::services::provider::compact_resumed_provider_turn_prompt(
            self.provider,
            self.session_id,
            chunks.join("\n\n"),
        )
    }
}
