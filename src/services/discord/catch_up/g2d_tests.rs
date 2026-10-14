//! Behavior of dormant history coverage and the shared intake adapter.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::{ChannelId, Message, MessageId};
use sha2::Digest;

use super::api::{CatchUpDiscordApi, CatchUpFetchRequest};
use super::ledger::{self, ClassifiedSource, Coverage, Incomplete, LedgerFetchResult};
use crate::services::cluster::attachment_transfer::AttachmentEntryV1;
use crate::services::discord::input_runtime::intake::{
    self, Classification, InputDisposition, Origin, PolicyFacts, Protection, Refusal, Retry,
    SourceDraft, StagedAttachment, TargetFacts, VoiceTarget,
};
use crate::services::discord::{RuntimeChannelBindingStatus, SharedData};
use crate::services::provider::ProviderKind;
use crate::voice::prompt::VoiceTranscriptAnnouncement;

type Page = (CatchUpFetchRequest, Result<Vec<u64>, String>);

struct Pages(Mutex<VecDeque<Page>>);

impl Pages {
    fn new(pages: Vec<Page>) -> Self {
        Self(Mutex::new(pages.into()))
    }

    fn consumed(&self) {
        assert!(
            self.0.lock().unwrap().is_empty(),
            "all expected requests ran"
        );
    }
}

#[async_trait::async_trait]
impl CatchUpDiscordApi for Pages {
    async fn current_user_id(&self) -> Result<Option<u64>, String> {
        panic!("fetch must not resolve policy");
    }

    async fn resolve_runtime_channel_binding_status(
        &self,
        _: ChannelId,
    ) -> RuntimeChannelBindingStatus {
        panic!("fetch must not resolve policy");
    }

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        request: CatchUpFetchRequest,
    ) -> Result<Vec<Message>, String> {
        assert_eq!(channel_id.get(), 8);
        let (expected, page) = self.0.lock().unwrap().pop_front().expect("bounded fetch");
        assert_eq!(request, expected);
        page.map(|ids| ids.into_iter().map(message).collect())
    }

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        _: &Arc<SharedData>,
        _: ChannelId,
        _: MessageId,
    ) {
        panic!("fetch must not emit effects");
    }
}

fn message(id: u64) -> Message {
    serde_json::from_value(serde_json::json!({
        "id": id.to_string(), "channel_id": "8", "content": "raw",
        "author": {"id": "7", "username": "raw-author", "discriminator": "0001", "bot": id.is_multiple_of(2)},
        "timestamp": "2026-10-14T00:00:00Z", "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false, "type": 0
    }))
    .unwrap()
}

fn latest(ids: Vec<u64>) -> Page {
    (CatchUpFetchRequest::new(1), Ok(ids))
}

fn before(cursor: u64, ids: Vec<u64>) -> Page {
    (CatchUpFetchRequest::new(50).before(cursor), Ok(ids))
}

fn ids(result: &LedgerFetchResult) -> Vec<u64> {
    result
        .messages
        .iter()
        .map(|message| message.id.get())
        .collect()
}

#[tokio::test]
async fn raw_pages_require_contiguous_coverage() {
    let channel = ChannelId::new(8);
    let mut first: Vec<_> = (102..=151).rev().collect();
    first.rotate_left(13);
    let mut second: Vec<_> = (100..=102).rev().collect();
    second.extend([152, 999, 101]);
    let api = Pages::new(vec![
        latest(vec![152]),
        before(152, first),
        before(102, second),
    ]);
    let result = ledger::fetch(&api, channel, Some(100), 4).await;
    assert_eq!(result.lower_exclusive, Some(100));
    assert_eq!(result.horizon, Some(152));
    assert_eq!(result.coverage, Coverage::Complete);
    assert!(result.complete_fetch());
    assert_eq!(ids(&result), (101..=152).collect::<Vec<_>>());
    assert_eq!(
        result
            .messages
            .iter()
            .filter(|message| message.author.bot)
            .count(),
        26
    );
    api.consumed();

    let cases = [
        (vec![latest(vec![152])], 1, Incomplete::BudgetExhausted),
        (
            vec![latest(vec![152]), before(152, vec![151, 150])],
            2,
            Incomplete::BudgetExhausted,
        ),
        (
            vec![latest(vec![152]), before(152, vec![152, 153])],
            3,
            Incomplete::CursorNotAdvancing,
        ),
        (
            vec![
                latest(vec![152]),
                before(152, vec![151, 150]),
                (
                    CatchUpFetchRequest::new(50).before(150),
                    Err("middle".into()),
                ),
            ],
            4,
            Incomplete::FetchFailed("middle".into()),
        ),
        (
            vec![(CatchUpFetchRequest::new(1), Err("snapshot".into()))],
            3,
            Incomplete::FetchFailed("snapshot".into()),
        ),
        (vec![], 0, Incomplete::BudgetExhausted),
    ];
    for (pages, budget, reason) in cases {
        let api = Pages::new(pages);
        let result = ledger::fetch(&api, channel, Some(100), budget).await;
        api.consumed();
        assert!(!result.complete_fetch(), "{reason:?}");
        if reason == Incomplete::FetchFailed("middle".into()) {
            assert_eq!(
                ids(&result),
                vec![150, 151, 152],
                "partial originals survive"
            );
            assert_eq!(result.horizon, Some(152));
        }
        assert_eq!(result.coverage, Coverage::Incomplete(reason));
    }

    let api = Pages::new(vec![
        latest(vec![500]),
        before(500, vec![300, 110]),
        before(110, vec![]),
    ]);
    let result = ledger::fetch(&api, channel, Some(100), 3).await;
    api.consumed();
    assert!(
        result.complete_fetch(),
        "empty history proves a deleted lower bound"
    );
    assert_eq!(
        ids(&result),
        vec![110, 300, 500],
        "snowflakes need not be consecutive"
    );

    for snapshot in [vec![], vec![99], vec![100]] {
        let api = Pages::new(vec![latest(snapshot)]);
        let result = ledger::fetch(&api, channel, Some(100), 1).await;
        api.consumed();
        assert!(result.complete_fetch());
        assert!(result.messages.is_empty());
    }
    for pages in [
        vec![latest(vec![])],
        vec![latest(vec![500]), before(500, vec![])],
    ] {
        let api = Pages::new(pages);
        let result = ledger::fetch(&api, channel, None, 3).await;
        api.consumed();
        assert_eq!(
            result.lower_exclusive, None,
            "no invented settlement prefix"
        );
        assert_eq!(
            result.coverage,
            Coverage::Incomplete(Incomplete::UnknownLowerBound)
        );
        assert!(!result.complete_fetch());
    }
}

fn draft() -> SourceDraft {
    SourceDraft {
        origin: Origin::Message {
            message_id: 10,
            source_ids: vec![10, 11],
            author_id: 7,
            original_channel_id: 8,
            execution_channel_id: 9,
        },
        author_is_bot: false,
        original_text: "use TextSkill".into(),
        prompt: "expanded TextSkill prompt".into(),
        reply_context: Some("prior reply".into()),
        attachments: vec![],
        voice_announcement: None,
    }
}

fn facts() -> PolicyFacts {
    PolicyFacts {
        provider: Some(ProviderKind::Claude),
        original: TargetFacts {
            channel_id: 8,
            protection: Protection::Unprotected,
        },
        execution: TargetFacts {
            channel_id: 9,
            protection: Protection::Protected(ProviderKind::Claude),
        },
        voice_target: VoiceTarget::NotApplicable,
        authorized: Some(true),
        supported: Some(true),
        disposition: Some(InputDisposition::Prompt),
        client_ready: true,
        admission_open: true,
    }
}

#[test]
fn classification_matches_common_intake() {
    let prepared = intake::prepare_source(draft()).unwrap();
    for case in 0..16 {
        let mut policy = facts();
        let expected = match case {
            0 => Classification::Target,
            1 => {
                policy.execution.protection = Protection::Unknown;
                Classification::Retry(Retry::Unknown)
            }
            2 => {
                policy.provider = Some(ProviderKind::Codex);
                Classification::Rejected(Refusal::ProviderMismatch)
            }
            3 => {
                policy.authorized = Some(false);
                Classification::Rejected(Refusal::Unauthorized)
            }
            4 => {
                policy.disposition = Some(InputDisposition::Terminal);
                Classification::NonTarget
            }
            5 => {
                policy.supported = Some(false);
                Classification::Rejected(Refusal::Unsupported)
            }
            6 => {
                policy.execution.protection = Protection::Unprotected;
                Classification::Legacy
            }
            7 => {
                policy.voice_target = VoiceTarget::Unknown;
                Classification::Retry(Retry::Unknown)
            }
            8 => {
                policy.authorized = None;
                Classification::Retry(Retry::Unknown)
            }
            9 => {
                policy.client_ready = false;
                Classification::Retry(Retry::ClientUnavailable)
            }
            10 => {
                policy.admission_open = false;
                Classification::Retry(Retry::Closed)
            }
            11 => {
                policy.execution.channel_id = 8;
                Classification::Rejected(Refusal::Provenance)
            }
            12 => {
                policy.provider = Some(ProviderKind::Gemini);
                policy.execution.protection = Protection::Protected(ProviderKind::Gemini);
                Classification::Rejected(Refusal::Unsupported)
            }
            13 => {
                policy.original.protection = Protection::Unknown;
                policy.disposition = Some(InputDisposition::Terminal);
                Classification::Retry(Retry::Unknown)
            }
            14 => {
                policy.voice_target = VoiceTarget::Unknown;
                policy.disposition = Some(InputDisposition::Terminal);
                Classification::Retry(Retry::Unknown)
            }
            _ => {
                policy.voice_target = VoiceTarget::Resolved(TargetFacts {
                    channel_id: 12,
                    protection: Protection::Protected(ProviderKind::Codex),
                });
                Classification::Rejected(Refusal::ProviderMismatch)
            }
        };
        assert_eq!(
            intake::classify(&policy, &prepared),
            expected,
            "C1 case {case}"
        );
        assert_eq!(
            ledger::classify_prepared(&policy, &prepared),
            expected,
            "D-a case {case}"
        );
        let adapted: ClassifiedSource = ledger::prepare(draft(), &policy).unwrap();
        assert_eq!(adapted.classification, expected);
        assert_eq!(adapted.source.identity(), prepared.identity());
        assert_eq!(adapted.source.input(), prepared.input());
    }

    for staged in [false, true] {
        let make = || {
            let mut source = draft();
            source.original_text.clear();
            source.prompt.clear();
            source.author_is_bot = true;
            source.attachments.push(StagedAttachment {
                filename: "image.png".into(),
                url: "https://example.invalid/image.png".into(),
                staged: staged.then(|| AttachmentEntryV1 {
                    filename: "image.png".into(),
                    sha256: hex::encode(sha2::Sha256::digest(b"bytes")),
                    bytes: b"bytes".to_vec(),
                }),
            });
            source.voice_announcement = Some(VoiceTranscriptAnnouncement {
                transcript: "spoken text".into(),
                user_id: "7".into(),
                utterance_id: "utterance".into(),
                language: "ko".into(),
                verbose_progress: false,
                started_at: None,
                completed_at: None,
                samples_written: Some(42),
                control_channel_id: Some(12),
                stt_mode: Some("local".into()),
                stt_latency_ms: Some(50),
            });
            source
        };
        let common = intake::prepare_source(make()).unwrap();
        let mut policy = facts();
        policy.voice_target = VoiceTarget::Resolved(TargetFacts {
            channel_id: 12,
            protection: Protection::Protected(ProviderKind::Claude),
        });
        let adapted = ledger::prepare(make(), &policy).unwrap();
        assert_eq!(
            adapted.classification,
            Classification::Target,
            "attachment-only {staged}"
        );
        assert_eq!(adapted.classification, intake::classify(&policy, &common));
        assert_eq!(adapted.source.identity(), common.identity());
        assert_eq!(adapted.source.input(), common.input());
        assert_eq!(adapted.source.attachments(), common.attachments());
        assert_eq!(adapted.source.attachments()[0].staged.is_some(), staged);
        assert_eq!(adapted.source.input()["reply_context"], "prior reply");
        assert_eq!(
            adapted.source.input()["voice_announcement"]["transcript"],
            "spoken text"
        );
    }
    let mut empty = draft();
    empty.original_text.clear();
    assert_eq!(
        ledger::prepare(empty, &facts()).unwrap().classification,
        Classification::Rejected(Refusal::Empty)
    );
    let mut invalid = draft();
    invalid.origin = Origin::Message {
        message_id: 0,
        source_ids: vec![],
        author_id: 7,
        original_channel_id: 8,
        execution_channel_id: 9,
    };
    assert!(ledger::prepare(invalid, &facts()).is_err());
}
