use super::*;
use crate::services::discord::router::response_format::retry_prompt_context::Preludes;
use serde_json::json;

#[test]
fn shared_payload_preserves_literal_text_attachment_only_and_author_bytes() {
    for raw in [
        "",
        "one\ntwo",
        "[WIP 리마인더] literal\n_",
        "인용\r\noriginal",
    ] {
        let uploads = vec!["file one".into(), "file two".into()];
        let prompt = TurnContext {
            provider: &ProviderKind::Claude,
            session_id: None,
            uploads: &uploads,
            trigger: Some("trigger".into()),
            reply: Some("raw reply"),
            recent: None,
            knowledge: Some("knowledge"),
            author_name: "name",
            author_id: UserId::new(42),
            user_input: raw.into(),
        }
        .build();
        let author = wrap_user_prompt_with_author("name", UserId::new(42), raw.into());
        assert_eq!(
            prompt,
            format!("file one\nfile two\n\ntrigger\n\nraw reply\n\nknowledge\n\n{author}"),
            "shared helper must preserve existing prompt bytes and order"
        );
        assert_eq!(prompt.matches("[User: name (ID: 42)]").count(), 1);
        assert_eq!(
            render_input_chunks(&uploads, Some("raw reply".into()), raw.into()),
            if raw.is_empty() {
                "file one\nfile two\n\nraw reply".into()
            } else {
                format!("file one\nfile two\n\nraw reply\n\n{raw}")
            }
        );
    }
}

#[test]
fn fresh_source_keeps_rawv1_selected_preludes_voice_and_assets_without_rendering() {
    let original: crate::services::turn_orchestrator::PendingQueueItem = serde_json::from_value(json!({
        "author_id":7,"message_id":100,"channel_id":7272,"source_message_ids":[100],
        "queued_generation":5,"text":"body\r\n_ literal WIP","pending_uploads":["file one","file two"], "voice_announcement":{
            "transcript":"voice", "user_id":"42", "utterance_id":"fresh", "language":"ko-KR", "verbose_progress":true,
            "started_at":null,"completed_at":null,"samples_written":null,"control_channel_id":null,"stt_mode":null,"stt_latency_ms":null}})).unwrap();
    let mut context = RetryPromptContext::raw(Some("quoted\r\n[WIP 리마인더] literal".into()));
    context.selected = Preludes {
        recovery: Some("user: prior".into()),
        feedback: Some("feedback".into()),
        wip: Some("dirty".into()),
    };
    context.expected_prompt = Some("already approved exact prompt".into());
    let before = serde_json::to_value(&original).unwrap();
    let fresh = fresh_source(&original, &context).unwrap();
    let restored = RetryPromptContext::restore(fresh.reply_context.clone());
    assert_eq!(
        restored, context,
        "constructor must retain the actual B carrier and approved selection"
    );
    assert_eq!(fresh.text, original.text);
    assert_eq!(fresh.pending_uploads, original.pending_uploads);
    assert_eq!(
        serde_json::to_value(&fresh.voice_announcement).unwrap(),
        serde_json::to_value(&original.voice_announcement).unwrap()
    );
    assert_eq!(serde_json::to_value(&original).unwrap(), before);
    let empty = fresh_source(&original, &RetryPromptContext::raw(None)).unwrap();
    assert!(
        RetryPromptContext::restore(empty.reply_context.clone()).may_select(),
        "empty RawV1 remains eligible without redefining B eligibility"
    );
    let carrier = build_race_requeued_intervention(
        UserId::new(7),
        MessageId::new(100),
        "raw",
        true,
        empty.reply_context,
        false,
        false,
        Vec::new(),
        None,
    );
    assert!(
        carrier.retry_prompt_context_is_empty(),
        "actual B empty-carrier eligibility remains unchanged"
    );
    let mut mixed = original;
    mixed.source_message_ids.push(200);
    assert!(
        fresh_source(&mixed, &context).is_err(),
        "a constructor cannot guess the provenance of a mixed envelope"
    );
}

#[cfg(unix)]
mod pg_tests;
