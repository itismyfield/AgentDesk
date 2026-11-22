//! The claimed send's stage reports, on the task response that holds its send the longest.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use super::claim_at_post_tests::owned;
use super::*;
use crate::domain::file_sync::PARENT_DIR_FSYNC_FLUSHES;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::cluster::session_matcher::{MatchedChannel, expected_rollout_path_for};
use crate::services::cluster::stream_relay::{self as relay, RelaySink, RelaySinkOutcome};
use crate::services::tui_o::cutover::{legacy_sends_report, test_override};
use crate::services::tui_o::store::{OStore, STORE_DIR_NAME, StoreConfig};
use crate::services::tui_o::writer::activation::activate_with;

/// The gates of one delivery: before its post lands, its settlement, and its return.
type Gates = [Arc<Semaphore>; 3];

fn gates() -> Gates {
    std::array::from_fn(|_| Arc::new(Semaphore::new(0)))
}

async fn pass(gate: &Semaphore) {
    gate.acquire().await.unwrap().forget();
}

/// A chunk sink whose post waits at its gate, then lands or fails.
struct Gated(Arc<Semaphore>, bool);

impl ResponseChunkTransport for Gated {
    async fn bot_user_id(&self) -> Result<u64, String> {
        Ok(77)
    }

    async fn post_chunk(
        &self,
        _channel_id: u64,
        _content: &str,
        _reference_message_id: Option<u64>,
        _nonce: &str,
    ) -> Result<u64, ResponseChunkPostError> {
        pass(&self.0).await;
        let refused = || ResponseChunkPostError::Permanent("refused".into());
        (!self.1).then_some(900_001).ok_or_else(refused)
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

/// One response as its sink delivers it: the chunks, then the sent-state settlement, then the
/// return that drops the claimed send.
async fn deliver(channel: u64, [post, settle, hold]: Gates, fail: bool) -> Result<(), String> {
    let claim = owned(channel).await;
    let sink = Gated(post, fail);
    let transport = claim_at_post(&sink, BodyClaim::new(channel, Some(ClaudeTui)));
    let sent = send_task_response_chunks(None, &transport, &claim, "ADK-8284 body").await;
    let Ok(BodySend::Sent(sent)) = transport.settle(sent) else {
        return Err("O holds the body".into());
    };
    sent.map_err(|error| error.to_string())?;
    let settlement = async {
        pass(&settle).await;
        super::super::record_task_response_sent_bounded(None, &claim).await
    };
    transport.settling(settlement).await?;
    pass(&hold).await;
    Ok(())
}

/// The channel's report once it shows `shown`.
async fn report(channel: u64, shown: &str) -> String {
    for _ in 0..2_000 {
        let report = legacy_sends_report(channel).unwrap();
        if report.contains(shown) {
            return report;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("{shown} never shown: {:?}", legacy_sends_report(channel));
}

/// A send blocked in its transport and one blocked in its settlement report different stages
/// under one claim order, and neither lets the channel's handoff through before the drop.
#[tokio::test]
async fn a_blocked_send_and_a_blocked_settlement_report_different_stages_and_refuse_handoff() {
    let channel = 8_284_001;
    let _candidates = test_override::force_candidates(&[(channel, ClaudeTui)]);
    let candidate = test_override::with_channels(|boot| boot?.candidate(channel).cloned()).unwrap();
    assert!(candidate.defer(channel));
    let root = tempfile::tempdir().unwrap();
    let store = if PARENT_DIR_FSYNC_FLUSHES {
        OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
            .unwrap()
            .unwrap()
    } else {
        // The handoff must refuse before writing to the store.
        std::fs::create_dir(root.path().join(STORE_DIR_NAME)).unwrap();
        OStore::existing(root.path()).unwrap()
    };
    let handoff = || {
        let (facts, sources) = (Ok(Default::default()), || Ok(Vec::new()));
        activate_with(&store, channel, facts, || Ok(false), &candidate, sources)
    };
    let gates = gates();
    let owner = tokio::spawn(deliver(channel, gates.clone(), false));
    for (gate, stage, shown) in [
        (0, "before_send", "sent=0 settled=0 finished=0"),
        (1, "sent_unsettled", "sent=1 settled=0 finished=0"),
        (2, "settled_undropped", "sent=1 settled=1 finished=0"),
    ] {
        let seen = report(channel, &format!("stage={stage}")).await;
        let refused = Err("Legacy body send in flight".to_string());
        assert_eq!(handoff(), refused, "a handoff during {stage}");
        assert!(seen.contains(shown), "{seen}");
        test_override::assert_first_open_send(&seen, Path::new(file!()), stage);
        gates[gate].add_permits(1);
    }
    assert_eq!(owner.await.unwrap(), Ok(()));
    report(
        channel,
        "started=1 sent=1 settled=1 finished=1 oldest_open=none",
    )
    .await;
}

/// The claimed send finishes when its owner returns an error and when its owner task is aborted
/// and joined, in the transport or in the settlement.
#[tokio::test]
async fn a_claimed_send_finishes_on_an_error_return_and_on_an_aborted_owner() {
    let cases = [
        (None, 1),
        (Some("before_send"), 0),
        (Some("sent_unsettled"), 1),
    ];
    for (index, (abort_at, sent)) in cases.into_iter().enumerate() {
        let channel = 8_284_010 + index as u64;
        let _candidates = test_override::force_candidates(&[(channel, ClaudeTui)]);
        let gates = gates();
        let owner = tokio::spawn(deliver(channel, gates.clone(), abort_at.is_none()));
        report(channel, "finished=0 oldest_open=seq=1").await;
        gates[0].add_permits((abort_at != Some("before_send")) as usize);
        if let Some(stage) = abort_at {
            report(channel, &format!("stage={stage}")).await;
            owner.abort();
            assert!(owner.await.unwrap_err().is_cancelled());
        } else {
            assert!(owner.await.unwrap().is_err());
        }
        let done = format!("started=1 sent={sent} settled=0 finished=1 oldest_open=none");
        report(channel, &done).await;
    }
}

/// A relay sink whose delivery is one gated response.
struct Relayed(u64, Gates);

#[async_trait::async_trait]
impl RelaySink for Relayed {
    async fn deliver(
        &self,
        _frame: &relay::StreamFrame,
    ) -> Result<RelaySinkOutcome, relay::RelaySinkError> {
        deliver(self.0, self.1.clone(), false).await.unwrap();
        Ok(RelaySinkOutcome::FrameAccepted)
    }
}

/// A relay told to shut down by its flag alone keeps the delivery under way, and with it the
/// claimed send, until that delivery ends by itself.
#[tokio::test]
async fn a_relay_shutdown_flag_keeps_the_claimed_send_until_its_delivery_ends() {
    let channel = 8_284_020;
    let _candidates = test_override::force_candidates(&[(channel, ClaudeTui)]);
    let session = format!("AgentDesk-claude-i8284-{channel}");
    let matched = MatchedChannel {
        channel_id: channel.to_string(),
        agent_id: "i8284".into(),
        provider: crate::services::provider::ProviderKind::Claude,
        expected_rollout_path: expected_rollout_path_for(&session),
        expected_session_name: session,
    };
    let gates = gates();
    let handle = relay::spawn_stream_relay(matched, Arc::new(Relayed(channel, gates.clone())));
    assert!(handle.try_send_frame("frame".into()));
    report(channel, "stage=before_send").await;
    let mut shutdown = Box::pin(handle.shutdown_with_result());
    let early = tokio::time::timeout(Duration::from_millis(50), &mut shutdown).await;
    assert!(
        early.is_err(),
        "the flag ended a delivery under way: {early:?}"
    );
    report(channel, "sent=0 settled=0 finished=0 oldest_open=seq=1").await;
    gates.iter().for_each(|gate| gate.add_permits(1));
    assert_eq!(shutdown.await.unwrap(), relay::ShutdownOutcome::Joined);
    report(
        channel,
        "started=1 sent=1 settled=1 finished=1 oldest_open=none",
    )
    .await;
}
