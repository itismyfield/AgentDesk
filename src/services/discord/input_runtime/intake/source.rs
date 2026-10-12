//! Original Discord provenance and attachment staging; preparation performs no persistence.

use std::io;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::services::cluster::attachment_transfer::AttachmentEntryV1;
use crate::services::discord::input_runtime::supervisor::{external, source::Source};
use crate::services::tui_input::rows::receipt_identity::ReceiptIdentity;
use crate::voice::prompt::VoiceTranscriptAnnouncement;

const SLASH_NAMESPACE: &str = "discord-slash-interaction-v1";

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct StagedAttachment {
    pub filename: String,
    pub url: String,
    pub staged: Option<AttachmentEntryV1>,
}

pub(in crate::services::discord) struct SourceDraft {
    pub origin: Origin,
    pub author_is_bot: bool,
    pub original_text: String,
    pub prompt: String,
    pub reply_context: Option<String>,
    pub attachments: Vec<StagedAttachment>,
    pub voice_announcement: Option<VoiceTranscriptAnnouncement>,
}

pub(in crate::services::discord) struct PreparedSource {
    key: u64,
    identity: ReceiptIdentity,
    input: Value,
    attachments: Vec<StagedAttachment>,
}

impl PreparedSource {
    pub fn from_material(key: u64, identity: ReceiptIdentity, input: Value) -> io::Result<Self> {
        let source = Source::new(key, identity, input, Vec::new())?;
        let input = source.input().clone();
        let invalid =
            || io::Error::new(io::ErrorKind::InvalidInput, "invalid attachment reference");
        let mut attachments = Vec::new();
        if let Some(references) = input.get("attachments") {
            for reference in references.as_array().ok_or_else(invalid)? {
                let filename = reference
                    .get("filename")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(invalid)?;
                let url = reference
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(invalid)?;
                attachments.push(StagedAttachment {
                    filename: filename.into(),
                    url: url.into(),
                    staged: None,
                });
            }
        }
        Ok(Self {
            key: source.key(),
            identity: source.identity().clone(),
            input,
            attachments,
        })
    }

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
    pub fn into_parts(self) -> (u64, ReceiptIdentity, Value, Vec<StagedAttachment>) {
        (self.key, self.identity, self.input, self.attachments)
    }
    pub fn has_content(&self) -> bool {
        !self.attachments.is_empty()
            || (self
                .input
                .get("original_text")
                .unwrap_or(&self.input["text"])
                .as_str()
                .is_some_and(|text| !text.trim().is_empty())
                && self.input["text"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()))
    }
}

/// Bytes remain staged for pinning under the existing receipt loan; preparation is not a receipt.
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
    #[cfg(test)]
    let input = {
        use crate::services::discord::input_runtime::supervisor::mutant;
        let mut input = input;
        if mutant("c2_author_bot_true") {
            input["author_is_bot"] = json!(true);
        }
        if mutant("c2_voice_transcript_empty") {
            input["voice_announcement"]["transcript"] = json!("");
        }
        if mutant("c2_voice_optional_removed")
            && let Some(voice) = input["voice_announcement"].as_object_mut()
        {
            voice.remove("stt_latency_ms");
        }
        input
    };
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
            origin: Origin::Message {
                message_id: 10,
                source_ids: vec![11, 10],
                author_id: 7,
                original_channel_id: 8,
                execution_channel_id: 9,
            },
            author_is_bot: true,
            original_text: "!skill explain".into(),
            prompt: "expanded skill prompt".into(),
            reply_context: Some("reply to original 5".into()),
            attachments: [b"first".as_slice(), b"second".as_slice()]
                .into_iter()
                .enumerate()
                .map(|(index, bytes)| {
                    let filename = format!("{index}.txt");
                    StagedAttachment {
                        filename: filename.clone(),
                        url: format!("https://fixture.invalid/{index}"),
                        staged: Some(AttachmentEntryV1 {
                            filename,
                            sha256: hex::encode(Sha256::digest(bytes)),
                            bytes: bytes.into(),
                        }),
                    }
                })
                .collect(),
            voice_announcement: Some(VoiceTranscriptAnnouncement {
                transcript: "spoken words".into(),
                user_id: "7".into(),
                utterance_id: "utterance-10".into(),
                language: "ko".into(),
                verbose_progress: true,
                started_at: Some("2026-10-12T08:00:00Z".into()),
                completed_at: Some("2026-10-12T08:00:02Z".into()),
                samples_written: Some(240),
                control_channel_id: Some(12),
                stt_mode: Some("streaming".into()),
                stt_latency_ms: Some(321),
            }),
        }
    }

    #[test]
    fn c1_rich_source_preserves_original_identity_text_reply_voice_and_all_staged_bytes() {
        for author_is_bot in [false, true] {
            let mut d = draft();
            d.author_is_bot = author_is_bot;
            let expected_identity = ReceiptIdentity::new(10, vec![10, 11], 7, 8, 9).unwrap();
            let expected_voice = d.voice_announcement.clone();
            let expected_attachments = d.attachments.clone();
            let references: Vec<_> = d
                .attachments
                .iter()
                .map(|attachment| json!({"filename":attachment.filename,"url":attachment.url}))
                .collect();
            let expected_input = json!({
                "message_id":10, "source_message_ids":expected_identity.source_ids,
                "author_id":7, "original_channel_id":8, "execution_channel_id":9, "channel_id":9,
                "receipt_identity":expected_identity, "author_is_bot":author_is_bot,
                "original_text":d.original_text, "text":d.prompt, "reply_context":d.reply_context,
                "attachments":references, "voice_announcement":expected_voice, "http_origin":null,
            });
            let source = prepare_source(d).unwrap();
            assert_eq!(source.key(), 10);
            assert_eq!(source.identity(), &expected_identity);
            assert_eq!(source.input(), &expected_input);
            assert_eq!(
                serde_json::from_value::<Option<VoiceTranscriptAnnouncement>>(
                    source.input()["voice_announcement"].clone()
                )
                .unwrap(),
                expected_voice
            );
            assert_eq!(source.attachments(), expected_attachments.as_slice());
            assert!(source.has_content());
            let (key, identity, input, attachments) = source.into_parts();
            assert_eq!(key, 10);
            assert_eq!(identity, expected_identity);
            assert_eq!(input, expected_input);
            assert_eq!(attachments, expected_attachments);
            let source = Source::new(key, identity, input, Vec::new()).unwrap();
            assert_eq!(source.input(), &expected_input);
        }
    }

    #[test]
    fn c2_material_roundtrip_retains_full_canonical_input_and_unfetched_references() {
        let prepared = prepare_source(draft()).unwrap();
        let mut input = prepared.input().clone();
        let mut identity = prepared.identity().clone();
        identity.source_ids.reverse();
        input["receipt_identity"] = json!(identity);
        input["opaque_metadata"] = json!({"nested":[1,{"future":"preserved"}]});
        input["attachments"][0]["content_type"] = json!("text/plain");
        let mut expected_input = input.clone();
        expected_input["receipt_identity"] = json!(prepared.identity());
        let expected_identity = prepared.identity().clone();
        let expected_attachments: Vec<_> = prepared
            .attachments()
            .iter()
            .cloned()
            .map(|mut attachment| {
                attachment.staged = None;
                attachment
            })
            .collect();
        let material = PreparedSource::from_material(prepared.key(), identity, input).unwrap();
        assert_eq!(material.identity(), &expected_identity);
        assert_eq!(material.input(), &expected_input);
        assert_eq!(material.attachments(), expected_attachments.as_slice());
        assert!(material.has_content());
        let (key, identity, input, attachments) = material.into_parts();
        assert_eq!(key, 10);
        assert_eq!(identity, expected_identity);
        assert_eq!(input, expected_input);
        assert_eq!(attachments, expected_attachments);
        let source = Source::new(key, identity, input, Vec::new()).unwrap();
        assert_eq!(source.input(), &expected_input);
    }

    #[test]
    fn c2_material_text_uses_original_when_present_and_metadata_alone_is_not_content() {
        let identity = ReceiptIdentity::new(10, vec![10], 7, 8, 9).unwrap();
        for (original_text, text, content) in [
            (None, "actual actor request", true),
            (None, " \n", false),
            (Some(json!("original request")), "rendered request", true),
            (Some(json!("")), "wrapper boilerplate", false),
            (Some(json!(" \n")), "wrapper boilerplate", false),
            (Some(Value::Null), "wrapper boilerplate", false),
            (Some(json!(12)), "wrapper boilerplate", false),
            (Some(json!("original request")), " \n", false),
        ] {
            let mut input = json!({
                "text":text, "reply_context":"reply is not a request", "attachments":[],
                "voice_announcement":draft().voice_announcement,
            });
            if let Some(original_text) = original_text {
                input["original_text"] = original_text;
            }
            let source = PreparedSource::from_material(10, identity.clone(), input).unwrap();
            assert_eq!(source.has_content(), content);
            assert!(source.attachments().is_empty());
        }
        let source = PreparedSource::from_material(10, identity, json!({"text":""})).unwrap();
        assert!(!source.has_content());
        assert!(source.attachments().is_empty());
    }

    #[test]
    fn c2_material_attachment_refs_are_content_without_claiming_staged_bytes() {
        let identity = ReceiptIdentity::new(10, vec![10], 7, 8, 9).unwrap();
        let input = json!({"text":"", "original_text":"", "attachments":[
            {"filename":"note.txt","url":"https://fixture.invalid/note","bytes":"untrusted"},
            {"filename":"image.png","url":"https://fixture.invalid/image"},
        ]});
        let source = PreparedSource::from_material(10, identity, input.clone()).unwrap();
        assert!(source.has_content());
        assert_eq!(source.input()["attachments"], input["attachments"]);
        assert_eq!(
            source.attachments(),
            &[
                StagedAttachment {
                    filename: "note.txt".into(),
                    url: "https://fixture.invalid/note".into(),
                    staged: None
                },
                StagedAttachment {
                    filename: "image.png".into(),
                    url: "https://fixture.invalid/image".into(),
                    staged: None
                },
            ]
        );
    }

    #[test]
    fn c2_material_rejects_malformed_attachment_fields_refs_and_source_provenance() {
        let identity = ReceiptIdentity::new(10, vec![10], 7, 8, 9).unwrap();
        for attachments in [
            Value::Null,
            json!({}),
            json!("not refs"),
            json!([null]),
            json!([12]),
            json!([[]]),
            json!([{}]),
            json!([{"filename":"note.txt"}]),
            json!([{"url":"https://fixture.invalid/note"}]),
            json!([{"filename":null,"url":"https://fixture.invalid/note"}]),
            json!([{"filename":12,"url":"https://fixture.invalid/note"}]),
            json!([{"filename":"","url":"https://fixture.invalid/note"}]),
            json!([{"filename":" \n","url":"https://fixture.invalid/note"}]),
            json!([{"filename":"note.txt","url":null}]),
            json!([{"filename":"note.txt","url":12}]),
            json!([{"filename":"note.txt","url":""}]),
            json!([{"filename":"note.txt","url":" \n"}]),
        ] {
            assert!(
                PreparedSource::from_material(
                    10,
                    identity.clone(),
                    json!({
                        "text":"request", "attachments":attachments,
                    })
                )
                .is_err()
            );
        }
        for input in [
            Value::Null,
            json!({}),
            json!({"text":12}),
            json!({"text":"request","message_id":11}),
            json!({"text":"request","author_id":8}),
            json!({"text":"request","source_message_ids":[11]}),
            json!({"text":"request","receipt_identity":{"source_ids":[10],"author_id":8,
                "original_channel_id":8,"execution_channel_id":9}}),
        ] {
            assert!(PreparedSource::from_material(10, identity.clone(), input).is_err());
        }
        assert!(PreparedSource::from_material(11, identity, json!({"text":"request"})).is_err());
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
