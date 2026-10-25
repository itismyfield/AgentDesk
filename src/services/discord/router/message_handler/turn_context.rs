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
        if let Some(trigger) = self.trigger {
            chunks.push(trigger);
        }
        if let Some(recent) = self.recent {
            recent.append_rendered_context_to(&mut chunks);
        }
        if let Some(knowledge) = self.knowledge {
            chunks.push(knowledge.to_string());
        }
        if let Some(reply) = self.reply {
            chunks.push(reply.to_string());
        }
        let input = wrap_user_prompt_with_author(self.author_name, self.author_id, self.user_input);
        crate::services::provider::compact_resumed_provider_turn_prompt(
            self.provider,
            self.session_id,
            render_input_chunks(self.uploads, chunks, input),
        )
    }
}

/// Input chunks have one ordering regardless of the transport's outer author frame.
pub(in crate::services::discord::router) fn render_input_chunks(
    uploads: &[String],
    context: impl IntoIterator<Item = String>,
    user_input: String,
) -> String {
    let mut chunks = Vec::new();
    if !uploads.is_empty() {
        chunks.push(uploads.join("\n"));
    }
    chunks.extend(context);
    if !user_input.is_empty() {
        chunks.push(user_input);
    }
    chunks.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_chunks_preserve_plain_reply_and_upload_order() {
        assert_eq!(render_input_chunks(&[], None, "raw".into()), "raw");
        assert_eq!(
            render_input_chunks(&[], Some("reply".into()), "one\ntwo".into()),
            "reply\n\none\ntwo"
        );
        assert_eq!(
            render_input_chunks(
                &["file one".into(), "file two".into()],
                Some("reply".into()),
                "text".into()
            ),
            "file one\nfile two\n\nreply\n\ntext"
        );
    }

    #[test]
    fn normal_turn_context_preserves_existing_author_and_context_bytes() {
        let prompt = TurnContext {
            provider: &ProviderKind::Claude,
            session_id: Some("session"),
            uploads: &["first".into(), "second".into()],
            trigger: Some("trigger".into()),
            reply: Some("reply"),
            recent: None,
            knowledge: Some("knowledge"),
            author_name: "name",
            author_id: UserId::new(42),
            user_input: "one\ntwo".into(),
        }
        .build();
        assert_eq!(
            prompt,
            "first\nsecond\n\ntrigger\n\nknowledge\n\nreply\n\n[User: name (ID: 42)]\none\ntwo"
        );
    }

    #[test]
    fn normal_attachment_only_turn_keeps_its_nonempty_author_chunk() {
        let prompt = TurnContext {
            provider: &ProviderKind::Claude,
            session_id: None,
            uploads: &["file record".into()],
            trigger: None,
            reply: None,
            recent: None,
            knowledge: None,
            author_name: "name",
            author_id: UserId::new(42),
            user_input: String::new(),
        }
        .build();
        assert_eq!(prompt, "file record\n\n[User: name (ID: 42)]");
    }

    #[test]
    fn reply_context_is_last_before_user_input_with_recent_and_knowledge() {
        let recent = ChannelRecentContextManifestInput {
            rendered_context: "[이전 대화 복원]\nrecent user and assistant".into(),
            pair_count: 1,
            audit_reason: "fresh_session;pairs=1".into(),
        };
        let reply = "[Reply context — replied-to message]\nAuthor: bot\nContent: answer\n[End of reply context — the user's reply follows]";
        let prompt = TurnContext {
            provider: &ProviderKind::Codex,
            session_id: None,
            uploads: &[],
            trigger: Some("trigger".into()),
            reply: Some(reply),
            recent: Some(&recent),
            knowledge: Some("knowledge"),
            author_name: "name",
            author_id: UserId::new(42),
            user_input: "one\ntwo".into(),
        }
        .build();
        let (_, after_reply) = prompt
            .split_once("[End of reply context — the user's reply follows]")
            .unwrap();
        assert_eq!(after_reply, "\n\n[User: name (ID: 42)]\none\ntwo");
        assert!(prompt.starts_with(&format!(
            "trigger\n\n{}\n\nknowledge\n\n{reply}",
            recent.rendered_context
        )));
    }
}
