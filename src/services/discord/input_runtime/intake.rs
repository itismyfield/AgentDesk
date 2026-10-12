//! Shared source and policy boundaries for the existing supervisor, with no production start.

#[path = "intake/classify.rs"]
mod classify;
#[path = "intake/client.rs"]
pub(in crate::services::discord) mod client;
#[path = "intake/source.rs"]
mod source;

pub(in crate::services::discord) use classify::{
    Classification, InputDisposition, PolicyFacts, Protection, Refusal, Retry, TargetFacts,
    VoiceTarget, classify,
};
pub(in crate::services::discord) use client::Client;
#[cfg(test)]
pub(in crate::services::discord) use client::{Access, Applied, apply_from_scan, lookup};
pub(in crate::services::discord) use source::{
    Origin, PreparedSource, SourceDraft, StagedAttachment, prepare_source,
};

#[cfg(test)]
#[test]
fn c1_public_boundary_keeps_receipt_and_submit_readiness_separate() {
    let source = prepare_source(SourceDraft {
        origin: Origin::Message {
            message_id: 10,
            source_ids: vec![10],
            author_id: 7,
            original_channel_id: 8,
            execution_channel_id: 9,
        },
        author_is_bot: false,
        original_text: "hello".into(),
        prompt: "hello".into(),
        reply_context: None,
        attachments: Vec::<StagedAttachment>::new(),
        voice_announcement: None,
    })
    .unwrap();
    let _prepared: &PreparedSource = &source;
    assert_eq!(source.key(), 10);
    assert_eq!(source.identity().source_ids, vec![10]);
    assert_eq!(source.input()["original_text"], "hello");
    assert!(source.attachments().is_empty());
    let target = |channel_id| TargetFacts {
        channel_id,
        protection: Protection::Protected(crate::services::provider::ProviderKind::Claude),
    };
    let mut facts = PolicyFacts {
        provider: Some(crate::services::provider::ProviderKind::Claude),
        original: target(8),
        execution: target(9),
        voice_target: VoiceTarget::NotApplicable,
        authorized: Some(true),
        supported: Some(true),
        disposition: Some(InputDisposition::Prompt),
        client_ready: true,
        admission_open: true,
    };
    assert_eq!(classify(&facts, &source), Classification::Target);
    facts.admission_open = false;
    assert_eq!(
        classify(&facts, &source),
        Classification::Retry(Retry::Closed)
    );
    facts.authorized = Some(false);
    assert_eq!(
        classify(&facts, &source),
        Classification::Rejected(Refusal::Unauthorized)
    );
}
