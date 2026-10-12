//! Original Discord provenance and attachment staging; preparation performs no persistence.

#[cfg(test)]
use std::io;

#[cfg(test)]
use serde_json::{Value, json};
#[cfg(test)]
use sha2::{Digest, Sha256};

#[cfg(test)]
use crate::services::cluster::attachment_transfer::AttachmentEntryV1;
#[cfg(test)]
use crate::services::discord::input_runtime::supervisor::{external, source::Source};
#[cfg(test)]
use crate::services::tui_input::rows::receipt_identity::ReceiptIdentity;
#[cfg(test)]
use crate::voice::prompt::VoiceTranscriptAnnouncement;

#[cfg(test)]
const SLASH_NAMESPACE: &str = "discord-slash-interaction-v1";

#[cfg(test)]
pub(in crate::services::discord) enum Origin {
    Message {
        message_id: u64,
        source_ids: Vec<u64>,
        author_id: u64,
        original_channel_id: u64,
        execution_channel_id: u64,
    },
    SlashInteraction {
        interaction_id: u64,
        author_id: u64,
        original_channel_id: u64,
        execution_channel_id: u64,
    },
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct StagedAttachment {
    pub filename: String,
    pub url: String,
    pub staged: Option<AttachmentEntryV1>,
}

#[cfg(test)]
pub(in crate::services::discord) struct SourceDraft {
    pub origin: Origin,
    pub author_is_bot: bool,
    pub original_text: String,
    pub prompt: String,
    pub reply_context: Option<String>,
    pub attachments: Vec<StagedAttachment>,
    pub voice_announcement: Option<VoiceTranscriptAnnouncement>,
}

#[cfg(test)]
pub(in crate::services::discord) struct PreparedSource {
    key: u64,
    identity: ReceiptIdentity,
    input: Value,
    attachments: Vec<StagedAttachment>,
}

#[cfg(test)]
impl PreparedSource {
    pub fn key(&self) -> u64 {
        self.key
    }
    pub fn identity(&self) -> &ReceiptIdentity {
        &self.identity
    }
    pub fn input(&self) -> &Value {
        &self.input
    }
    pub fn attachments(&self) -> &[StagedAttachment] {
        &self.attachments
    }
    pub fn has_content(&self) -> bool {
        !self.attachments.is_empty()
            || (self.input["original_text"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty())
                && self.input["text"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()))
    }
}

/// Bytes remain staged for pinning under the existing receipt loan; preparation is not a receipt.
#[cfg(test)]
pub(in crate::services::discord) fn prepare_source(
    draft: SourceDraft,
) -> io::Result<PreparedSource> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid original source material",
        )
    };
    let (key, identity, origin) = match draft.origin {
        Origin::Message {
            message_id,
            source_ids,
            author_id,
            original_channel_id,
            execution_channel_id,
        } => (
            message_id,
            ReceiptIdentity::new(
                message_id,
                source_ids,
                author_id,
                original_channel_id,
                execution_channel_id,
            )?,
            Value::Null,
        ),
        Origin::SlashInteraction {
            interaction_id,
            author_id,
            original_channel_id,
            execution_channel_id,
        } => {
            if interaction_id == 0 {
                return Err(invalid());
            }
            let origin_id = interaction_id.to_string();
            let external = external::source(
                execution_channel_id,
                author_id,
                &draft.prompt,
                Some(SLASH_NAMESPACE),
                &origin_id,
            )?;
            let key = external.key();
            (
                key,
                ReceiptIdentity::new(
                    key,
                    vec![key],
                    author_id,
                    original_channel_id,
                    execution_channel_id,
                )?,
                json!({"version":1,"source":SLASH_NAMESPACE,"origin_id":origin_id}),
            )
        }
    };
    for attachment in &draft.attachments {
        if attachment.filename.trim().is_empty()
            || attachment.url.trim().is_empty()
            || attachment.staged.as_ref().is_some_and(|staged| {
                staged.filename != attachment.filename
                    || staged.sha256 != hex::encode(Sha256::digest(&staged.bytes))
            })
        {
            return Err(invalid());
        }
    }
    let references: Vec<_> = draft
        .attachments
        .iter()
        .map(|attachment| json!({"filename":attachment.filename,"url":attachment.url}))
        .collect();
    let input = json!({
        "message_id":key, "source_message_ids":identity.source_ids,
        "author_id":identity.author_id, "original_channel_id":identity.original_channel_id,
        "execution_channel_id":identity.execution_channel_id, "channel_id":identity.execution_channel_id,
        "receipt_identity":identity, "author_is_bot":draft.author_is_bot,
        "original_text":draft.original_text, "text":draft.prompt,
        "reply_context":draft.reply_context, "attachments":references,
        "voice_announcement":draft.voice_announcement, "http_origin":origin,
    });
    Source::new(key, identity.clone(), input.clone(), Vec::new())?;
    Ok(PreparedSource {
        key,
        identity,
        input,
        attachments: draft.attachments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> SourceDraft {
        SourceDraft {
            origin: Origin::Message { message_id: 10, source_ids: vec![11,10], author_id: 7,
                original_channel_id: 8, execution_channel_id: 9 },
            author_is_bot: true, original_text: "!skill explain".into(), prompt: "expanded skill prompt".into(),
            reply_context: Some("reply to original 5".into()),
            attachments: [b"first".as_slice(), b"second".as_slice()].into_iter().enumerate().map(|(index,bytes)| {
                let filename = format!("{index}.txt");
                StagedAttachment { filename: filename.clone(), url: format!("https://fixture.invalid/{index}"),
                    staged: Some(AttachmentEntryV1 { filename, sha256: hex::encode(Sha256::digest(bytes)), bytes: bytes.into() }) }
            }).collect(),
            voice_announcement: Some(serde_json::from_value(json!({"transcript":"spoken words","user_id":"7",
                "utterance_id":"utterance-10","language":"ko","verbose_progress":false,
                "started_at":null,"completed_at":null,"samples_written":240,"control_channel_id":12})).unwrap()),
        }
    }

    #[test]
    fn c1_rich_source_preserves_original_identity_text_reply_voice_and_all_staged_bytes() {
        let source = prepare_source(draft()).unwrap();
        assert_eq!(source.key, 10);
        assert_eq!(
            source.identity,
            ReceiptIdentity::new(10, vec![10, 11], 7, 8, 9).unwrap()
        );
        assert_eq!(source.input["author_is_bot"], true);
        assert_eq!(source.input["original_text"], "!skill explain");
        assert_eq!(source.input["text"], "expanded skill prompt");
        assert_eq!(source.input["reply_context"], "reply to original 5");
        assert_eq!(
            source.input["voice_announcement"]["utterance_id"],
            "utterance-10"
        );
        assert_eq!(source.input["voice_announcement"]["control_channel_id"], 12);
        assert_eq!(source.input["attachments"].as_array().unwrap().len(), 2);
        assert_eq!(source.attachments.len(), 2);
        for (index, bytes) in [b"first".as_slice(), b"second".as_slice()]
            .into_iter()
            .enumerate()
        {
            let staged = source.attachments[index].staged.as_ref().unwrap();
            assert_eq!(staged.bytes, bytes);
            assert_eq!(staged.sha256, hex::encode(Sha256::digest(bytes)));
            assert_eq!(
                source.input["attachments"][index]["url"],
                format!("https://fixture.invalid/{index}")
            );
        }
        assert!(source.has_content());
        let source = Source::new(source.key, source.identity, source.input, Vec::new()).unwrap();
        assert_eq!(source.key(), 10);
    }

    #[test]
    fn c1_source_rejects_missing_primary_zero_provenance_and_corrupt_staging() {
        for (key, ids, author, original, execution) in [
            (0, vec![0], 7, 8, 9),
            (10, vec![11], 7, 8, 9),
            (99, vec![10, 11], 7, 8, 9),
            (10, vec![10, 10], 7, 8, 9),
            (10, vec![10], 0, 8, 9),
            (10, vec![10], 7, 0, 9),
            (10, vec![10], 7, 8, 0),
        ] {
            let mut d = draft();
            d.origin = Origin::Message {
                message_id: key,
                source_ids: ids,
                author_id: author,
                original_channel_id: original,
                execution_channel_id: execution,
            };
            assert!(prepare_source(d).is_err());
        }
        for index in 0..4 {
            let mut d = draft();
            match index {
                0 => d.attachments[0].url.clear(),
                1 => d.attachments[0].filename.clear(),
                2 => d.attachments[0].staged.as_mut().unwrap().bytes.push(0),
                _ => d.attachments[0].staged.as_mut().unwrap().filename = "replacement".into(),
            }
            assert!(prepare_source(d).is_err());
        }
    }

    #[test]
    fn c1_slash_origin_is_fixed_actual_interaction_and_keeps_original_execution_channels() {
        let make = |interaction_id| {
            let mut d = draft();
            d.origin = Origin::SlashInteraction {
                interaction_id,
                author_id: 7,
                original_channel_id: 8,
                execution_channel_id: 9,
            };
            prepare_source(d)
        };
        assert!(make(0).is_err());
        let first = make(123).unwrap();
        assert_eq!(
            first.key,
            external::source(9, 7, "anything", Some(SLASH_NAMESPACE), "123")
                .unwrap()
                .key()
        );
        assert_eq!(first.key, make(123).unwrap().key());
        assert_ne!(first.key, make(124).unwrap().key());
        assert_eq!(
            first.identity,
            ReceiptIdentity::new(first.key, vec![first.key], 7, 8, 9).unwrap()
        );
        assert_eq!(
            first.input["http_origin"],
            json!({"version":1,"source":SLASH_NAMESPACE,"origin_id":"123"})
        );
        assert_eq!(first.input["message_id"], first.key);
    }

    #[test]
    fn c1_attachment_only_is_content_but_reply_or_voice_metadata_and_boilerplate_are_not() {
        let mut d = draft();
        d.original_text.clear();
        d.prompt.clear();
        d.attachments[0].staged = None;
        let source = prepare_source(d).unwrap();
        assert!(source.has_content());
        assert!(
            source.attachments[0].staged.is_none(),
            "unfetched reference stays unfetched"
        );
        let mut d = draft();
        d.original_text = " \n".into();
        d.attachments.clear();
        assert!(!prepare_source(d).unwrap().has_content());
        let mut d = draft();
        d.attachments.clear();
        d.prompt.clear();
        assert!(!prepare_source(d).unwrap().has_content());
    }
}
