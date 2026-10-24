//! Shared ordering for user context; attachment lifetime remains with the provider invocation.
use super::*;
use crate::services::discord::prompt_builder::ChannelRecentContextManifestInput;

pub(in crate::services::discord) struct TurnContext<'a> {
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
        super::input_payload::render_turn(self)
    }
}

pub(in crate::services::discord::router) use super::input_payload::render_input_chunks;

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
            "first\nsecond\n\ntrigger\n\nreply\n\nknowledge\n\n[User: name (ID: 42)]\none\ntwo"
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
}
