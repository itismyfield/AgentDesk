use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::store::backdate_response_chunk_post_for_test;
use super::super::{ResponseDeliveryClaimOutcome, ResponseDeliveryOwner};
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::tui_o::channel_policy::{Adoption, BodyCheck, SinkOp};
use crate::services::tui_o::cutover::test_override;

const BODY: &str = "ADK-C1A-claim-at-post-body";
const BOT: u64 = 77_6325;

/// A chunk sink that shows each post to the adoption check as it lands; history is empty.
struct Sink {
    check: BodyCheck,
    bot: Result<u64, String>,
    posts: AtomicUsize,
}

impl ResponseChunkTransport for Sink {
    async fn bot_user_id(&self) -> Result<u64, String> {
        self.bot.clone()
    }

    async fn post_chunk(
        &self,
        channel_id: u64,
        content: &str,
        _reference_message_id: Option<u64>,
        _nonce: &str,
    ) -> Result<u64, ResponseChunkPostError> {
        self.check.sink(channel_id, SinkOp::Post, content);
        Ok(900_000 + self.posts.fetch_add(1, Ordering::AcqRel) as u64)
    }

    async fn history_page(
        &self,
        _channel_id: u64,
        _before_message_id: Option<u64>,
        _limit: usize,
    ) -> Result<Vec<ResponseChunkHistoryMessage>, ResponseChunkHistoryError> {
        Ok(Vec::new())
    }
}

async fn owned(channel: u64) -> ResponseDeliveryClaim {
    let session = format!("AgentDesk-claude-c1a-post-{channel}");
    let turn_key = super::super::durable_response_turn_key(
        channel, "claude", &session, 0, "", None, 4_300, BODY,
    );
    let claim = super::super::claim_task_response_delivery(
        None,
        channel,
        "claude",
        &session,
        "c1a-post-event",
        &turn_key,
        channel + 1,
        ResponseDeliveryOwner::Sink,
    )
    .await;
    let Ok(ResponseDeliveryClaimOutcome::Owned(claim)) = claim else {
        panic!("a fresh response is owned: {claim:?}");
    };
    claim
}

/// Journals the response's only chunk as a send attempt would, with `hash` as its content.
async fn prepared(claim: &ResponseDeliveryClaim, hash: &str) -> PreparedResponseChunk {
    let nonce = response_chunk_nonce_for_generation(
        claim.response_turn_key(),
        claim.response_generation(),
        0,
    );
    let reference = Some(claim.card_message_id());
    let journal = prepare_response_chunk(None, claim, 0, 1, hash, &nonce, BOT, reference).await;
    let Ok(ResponseChunkJournal::Prepared(prepared)) = journal else {
        panic!("a fresh chunk is prepared: {journal:?}");
    };
    prepared
}

async fn posting(claim: &ResponseDeliveryClaim) -> PreparedResponseChunk {
    let hash = content_hash(&crate::services::discord::formatting::split_message(BODY)[0]);
    let prepared = prepared(claim, &hash).await;
    mark_response_chunk_posting(None, claim, &prepared)
        .await
        .unwrap()
}

/// Every no-post decision (identity lookup, journal conflict, confirmed or quarantined chunk,
/// history that cannot prove absence) leaves a pending adoption; only a real post releases it.
#[tokio::test]
async fn a_task_response_claims_its_channel_only_at_a_chunk_post() {
    let cases = [
        ("identity", 4_325_420u64),
        ("conflict", 4_325_421),
        ("confirmed", 4_325_422),
        ("quarantined", 4_325_423),
        ("history", 4_325_424),
        ("sent", 4_325_425),
    ];
    let channels: Vec<_> = cases.iter().map(|&(_, ch)| (ch, ClaudeTui)).collect();
    let _candidates = test_override::force_candidates(&channels);
    for (case, channel) in cases {
        let claim = owned(channel).await;
        match case {
            "conflict" => drop(prepared(&claim, "another body").await),
            "confirmed" => {
                let posting = posting(&claim).await;
                confirm_response_chunk(None, &claim, &posting, 55)
                    .await
                    .unwrap();
            }
            "quarantined" => {
                let posting = posting(&claim).await;
                mark_response_chunk_ambiguous(None, &claim, &posting, "lost reply")
                    .await
                    .unwrap();
            }
            "history" => {
                drop(posting(&claim).await);
                backdate_response_chunk_post_for_test(&claim, 0, 600);
            }
            _ => {}
        }
        let check = BodyCheck::watch(channel, BODY);
        let bot = match case {
            "identity" => Err("no bot user".to_string()),
            _ => Ok(BOT),
        };
        let sink = Sink {
            check: check.clone(),
            bot,
            posts: AtomicUsize::new(0),
        };
        let transport = claim_at_post(&sink, BodyClaim::new(channel, Some(ClaudeTui)));
        let sent = send_task_response_chunks(None, &transport, &claim, BODY).await;
        let posts = sink.posts.load(Ordering::Acquire);
        let sent = transport.settle(sent);
        check.assert_settled();
        let result = match sent {
            Ok(BodySend::Sent(result)) => result,
            other => panic!("{case}: Legacy owns a pending channel: {other:?}"),
        };
        match case {
            "sent" => {
                assert_eq!((posts, check.adoption()), (1, Adoption::Released), "{case}");
                assert_eq!(result.map(|ids| ids.len()).ok(), Some(1), "{case}");
            }
            "confirmed" => {
                assert_eq!((posts, check.adoption()), (0, Adoption::Pending), "{case}");
                assert_eq!(result.ok(), Some(vec![MessageId::new(55)]), "{case}");
            }
            _ => {
                assert_eq!((posts, check.adoption()), (0, Adoption::Pending), "{case}");
                let reason = match &result {
                    Err(ResponseChunkDeliveryError::Transient(reason)) => reason.as_str(),
                    Err(ResponseChunkDeliveryError::Permanent(reason)) => reason.as_str(),
                    Err(ResponseChunkDeliveryError::Ambiguous { reason }) => reason.as_str(),
                    other => panic!("{case}: {other:?}"),
                };
                let expected = match case {
                    "identity" => "no bot user",
                    "conflict" => "differs from the durable journal",
                    "quarantined" => "quarantined",
                    _ => "empty history",
                };
                assert!(reason.contains(expected), "{case}: {reason}");
            }
        }
    }
}

/// When O has committed the channel, the first chunk post sends nothing and the send reports O.
#[tokio::test]
async fn a_task_response_posts_nothing_on_a_channel_o_committed() {
    let channel = 4_325_426;
    let _channels = test_override::force_channels(&[(channel, ClaudeTui)]);
    let claim = owned(channel).await;
    let check = BodyCheck::watch(channel, BODY);
    let sink = Sink {
        check: check.clone(),
        bot: Ok(BOT),
        posts: AtomicUsize::new(0),
    };
    let transport = claim_at_post(&sink, BodyClaim::new(channel, Some(ClaudeTui)));
    let sent = send_task_response_chunks(None, &transport, &claim, BODY).await;
    assert!(sent.is_err(), "{sent:?}");
    assert_eq!(sink.posts.load(Ordering::Acquire), 0);
    assert!(matches!(transport.settle(sent), Ok(BodySend::OwnedByO)));
    assert_eq!(check.adoption(), Adoption::Committed);
}
