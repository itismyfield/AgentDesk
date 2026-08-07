use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::{Value, json};
use serenity::http::Http;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::DeliveryJournalMode;
use crate::services::discord::SharedData;
use crate::services::discord::outbound::DiscordTransportReceipt;

use super::RelaySinkError;
use super::delivery_frontier::SinkDeliveryProofResult;

pub(in crate::services::discord) mod controller;
mod pg_store;
pub(in crate::services::discord) mod recovery;
pub(in crate::services::discord) mod watcher;

const JOURNAL_NAMESPACE: Uuid = Uuid::from_u128(0xd9829c0b_8692_4ef0_9396_f7d83aa84dd5);
const MAILBOX_CAPACITY: usize = 256;

/// The one observer per process.
///
/// Q1 puts every PG append behind a single actor per process. The sink used to
/// own its instance outright; #5071 T1 S3a adds a second family (the watcher)
/// that reaches the journal from a free function with no sink in scope, so the
/// instance moved here and both families share the one mailbox and one actor.
static PROCESS_OBSERVER: std::sync::LazyLock<JournalObserver> =
    std::sync::LazyLock::new(JournalObserver::default);

pub(super) fn process_observer() -> &'static JournalObserver {
    &PROCESS_OBSERVER
}

/// Shadow admission — mode, pool, cohort — in the same order and with the same
/// meaning as the sink's `begin_fresh`.
///
/// It lived in `journal::watcher` while the watcher was the only family that
/// could not reach `begin_fresh`; #5071 T1 S4 adds the controller family, which
/// needs the identical gate, so it moved to the shared parent for the same
/// reason `PROCESS_OBSERVER` did in S3a. No behaviour change: the watcher call
/// sites now spell it `super::admit`.
pub(super) fn admit(
    shared: &SharedData,
    channel_id: ChannelId,
    obligation_id: Uuid,
) -> Option<sqlx::PgPool> {
    let runtime = crate::config_live_reload::current()?.runtime.clone();
    if runtime.delivery_journal_mode != DeliveryJournalMode::Shadow {
        return None;
    }
    let pool = shared.pg_pool.clone()?;
    let internal = runtime
        .delivery_journal_internal_channel_ids
        .iter()
        .any(|id| id == &channel_id.get().to_string());
    if !internal && cohort_bucket(obligation_id) >= runtime.delivery_journal_cohort_percent.min(100)
    {
        return None;
    }
    Some(pool)
}

/// The type is visible to the whole `discord` module so the watcher facade can
/// return what it emitted; every field stays private to `journal`, so no caller
/// outside can read or construct one.
#[derive(Clone)]
pub(in crate::services::discord) struct JournalEvent {
    event_id: Uuid,
    obligation_id: Uuid,
    attempt_id: Option<Uuid>,
    kind: &'static str,
    seq: i16,
    idempotency_key: Vec<u8>,
    canonical_payload: Value,
    receipt: Option<DiscordTransportReceipt>,
}

#[derive(Clone)]
pub(super) struct AttemptObservation {
    obligation_id: Uuid,
    attempt_id: Uuid,
    frontier: (u64, u64),
    pool: sqlx::PgPool,
}

struct AppendCommand {
    pool: sqlx::PgPool,
    events: Vec<JournalEvent>,
}

#[derive(Default)]
struct JournalCounters {
    accepted: AtomicU64,
    persisted: AtomicU64,
    duplicate_noop: AtomicU64,
    dropped: AtomicU64,
    pg_error: AtomicU64,
    invariant_conflict: AtomicU64,
}

/// Sink-owned, non-blocking ingress to the process-local single PG writer.
pub(super) struct JournalObserver {
    sender: Mutex<Option<mpsc::Sender<AppendCommand>>>,
    counters: Arc<JournalCounters>,
}

impl Default for JournalObserver {
    fn default() -> Self {
        Self {
            sender: Mutex::new(None),
            counters: Arc::new(JournalCounters::default()),
        }
    }
}

impl JournalObserver {
    pub(super) fn begin_fresh(
        &self,
        shared: &Arc<SharedData>,
        delivery: &super::SessionRelayDelivery,
    ) -> Option<AttemptObservation> {
        let runtime = crate::config_live_reload::current()?.runtime.clone();
        if runtime.delivery_journal_mode != DeliveryJournalMode::Shadow {
            return None;
        }
        let pool = shared.pg_pool.clone()?;
        let key = TuiObligationKey::capture(shared, delivery);
        let obligation_id = key.obligation_id();
        let internal = runtime
            .delivery_journal_internal_channel_ids
            .iter()
            .any(|id| id == &delivery.channel_id.to_string());
        if !internal
            && cohort_bucket(obligation_id) >= runtime.delivery_journal_cohort_percent.min(100)
        {
            return None;
        }
        let attempt_id = Uuid::new_v5(&obligation_id, b"attempt:0");
        let observation = AttemptObservation {
            obligation_id,
            attempt_id,
            frontier: key.frontier,
            pool: pool.clone(),
        };
        self.submit(AppendCommand {
            pool,
            events: vec![
                event(obligation_id, None, "O", 0, key.payload()),
                event(
                    obligation_id,
                    Some(attempt_id),
                    "A",
                    1,
                    json!({
                        "attempt": 0,
                        "frontier_start": key.frontier.0,
                        "frontier_end": key.frontier.1,
                    }),
                ),
            ],
        });
        Some(observation)
    }

    pub(super) fn finish_fresh(
        &self,
        attempt: AttemptObservation,
        receipt: DiscordTransportReceipt,
        committed: bool,
    ) -> Vec<JournalEvent> {
        let mismatch = receipt.requested_channel_id != receipt.returned_channel_id;
        let events = if committed && !mismatch {
            vec![
                transport_event(attempt.obligation_id, attempt.attempt_id, receipt),
                event(
                    attempt.obligation_id,
                    Some(attempt.attempt_id),
                    "C",
                    3,
                    json!({"frontier_start": attempt.frontier.0, "frontier_end": attempt.frontier.1}),
                ),
            ]
        } else {
            vec![event(
                attempt.obligation_id,
                Some(attempt.attempt_id),
                "U",
                2,
                json!({"reason": if mismatch { "channel_mismatch" } else { "commit_not_persisted" },
                       "requested_channel_id": receipt.requested_channel_id,
                       "returned_channel_id": receipt.returned_channel_id,
                       "message_id": receipt.message_id}),
            )]
        };
        let emitted_events = events.clone();
        self.submit(AppendCommand {
            pool: attempt.pool,
            events,
        });
        emitted_events
    }

    fn submit(&self, command: AppendCommand) {
        let sender = self.sender();
        match sender.try_send(command) {
            Ok(()) => {
                self.counters.accepted.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn sender(&self) -> mpsc::Sender<AppendCommand> {
        let mut slot = self
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        slot.get_or_insert_with(|| {
            let (sender, mut receiver) = mpsc::channel::<AppendCommand>(MAILBOX_CAPACITY);
            let counters = self.counters.clone();
            super::super::task_supervisor::spawn_observed("delivery_journal_observer", async move {
                while let Some(command) = receiver.recv().await {
                    match pg_store::append_delivery_journal_batch(&command.pool, &command.events).await {
                        Ok(pg_store::AppendResult::Persisted) => &counters.persisted,
                        Ok(pg_store::AppendResult::DuplicateNoOp) => &counters.duplicate_noop,
                        Ok(pg_store::AppendResult::InvariantConflict) => &counters.invariant_conflict,
                        Err(error) => {
                            tracing::warn!(error = %error, "shadow delivery journal append failed");
                            &counters.pg_error
                        }
                    }.fetch_add(1, Ordering::Relaxed);
                }
            });
            sender
        }).clone()
    }
}

/// A pure predicate, so the route boundary is enforced by behaviour rather than
/// by where the call sits: `NewMessage` (index-0 delegation) and cutover
/// short-replace (S4) stay out of this family even if the call site moves.
/// Pinned by T1-T3.
pub(super) fn journals_sink_direct(
    route: &super::SessionBoundTerminalDeliveryRoute,
    cutover_short_replace: bool,
) -> bool {
    !cutover_short_replace
        && matches!(
            route,
            super::SessionBoundTerminalDeliveryRoute::PlaceholderEdit(_)
        )
}

/// D4.3: of a multi-chunk send, the message the legacy frontier commits is the
/// last one, so that is the receipt the `T` event carries. Pinned by T4.
pub(super) fn anchor_receipt(
    receipts: &[DiscordTransportReceipt],
) -> Option<DiscordTransportReceipt> {
    receipts.last().cloned()
}

/// Long-chunk send that also yields the anchor receipt. It lives here rather
/// than inline in `deliver_response` only because the anchor file is frozen at
/// the giant-file ratchet. The in-process `TurnGateway` (test-only) has no
/// Discord response to read a returned channel from, so it yields no receipt:
/// a synthesised one could never trip the `channel_mismatch` branch.
pub(super) async fn send_long_chunks_with_anchor_receipt(
    gateway: Option<&dyn super::super::gateway::TurnGateway>,
    http: &Http,
    channel: ChannelId,
    anchor: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(Vec<MessageId>, Option<DiscordTransportReceipt>), RelaySinkError> {
    if let Some(gateway) = gateway {
        return gateway
            .send_long_message_with_rollback(channel, anchor, text)
            .await
            .map(|message_ids| (message_ids, None))
            .map_err(RelaySinkError::Transient);
    }
    let receipts =
        super::super::formatting::send_long_message_raw_with_rollback_returning_receipts(
            http, channel, anchor, text, shared,
        )
        .await
        .map_err(|error| RelaySinkError::Transient(error.to_string()))?;
    let message_ids = super::super::formatting::message_ids_from_receipts(receipts.clone())
        .map_err(|error| RelaySinkError::Transient(error.to_string()))?;
    Ok((message_ids, anchor_receipt(&receipts)))
}

/// Settle only when both the observation and a receipt exist; a missing receipt
/// leaves the O/A observation dangling for shadow reconciliation rather than
/// fabricating a confirmation. Two invariants live here rather than at the three
/// sink call sites: the observation is consumed so a second settle emits nothing
/// (T6), and `committed` is derived from the frontier proof so only `Persisted`
/// yields `T`+`C` (T5).
pub(super) fn settle(
    observer: &JournalObserver,
    attempt: &mut Option<AttemptObservation>,
    receipt: Option<DiscordTransportReceipt>,
    proof: SinkDeliveryProofResult,
) -> Vec<JournalEvent> {
    let Some(receipt) = receipt else {
        return Vec::new();
    };
    let Some(attempt) = attempt.take() else {
        return Vec::new();
    };
    observer.finish_fresh(
        attempt,
        receipt,
        proof == SinkDeliveryProofResult::Persisted,
    )
}

struct TuiObligationKey {
    canonical: Vec<u8>,
    frontier: (u64, u64),
}

impl TuiObligationKey {
    fn capture(shared: &SharedData, delivery: &super::SessionRelayDelivery) -> Self {
        let path = shared
            .tmux_watchers
            .watcher_output_path(&delivery.session_name);
        let (path_hash, file_id) = source_coordinate(path.as_deref());
        let epoch_snapshot = shared
            .tmux_watchers
            .by_tmux_session
            .get(&delivery.session_name)
            .map(|handle| handle.pause_epoch.load(Ordering::Acquire))
            .unwrap_or(0);
        let generation = delivery.relay_generation_mtime_ns.unwrap_or(0);
        let reset_incarnation = shared
            .relay_frontier_token(poise::serenity_prelude::ChannelId::new(delivery.channel_id))
            .reset_incarnation;
        let start = delivery
            .relay_range
            .map(|range| range.0)
            .or(delivery.frame_turn_start_offset)
            .unwrap_or(0);
        let end = delivery
            .relay_range
            .map(|range| range.1)
            .or(delivery.terminal_consumed_end)
            .unwrap_or(start);
        // execution_id is derived from the watcher pause epoch plus durable source/turn
        // coordinates. The same inputs are node-independent; a restart that loses or
        // changes the epoch creates a new execution rather than falsely coalescing it.
        let execution_id = execution_id(
            &delivery.session_name,
            generation,
            epoch_snapshot,
            delivery.frame_turn_user_msg_id,
            &delivery.frame_turn_started_at,
            delivery.frame_turn_start_offset,
        );
        let mut bytes = Vec::new();
        for value in [
            delivery.provider.as_str(),
            &delivery.channel_id.to_string(),
            &delivery.session_name,
            &execution_id.to_string(),
        ] {
            push_field(&mut bytes, value);
        }
        bytes.extend_from_slice(&reset_incarnation.to_be_bytes());
        bytes.extend_from_slice(&generation.to_be_bytes());
        bytes.extend_from_slice(&path_hash.to_be_bytes());
        encode_file_id(file_id, &mut bytes);
        bytes.extend_from_slice(&start.to_be_bytes());
        bytes.extend_from_slice(&end.to_be_bytes());
        bytes.extend_from_slice(b"fresh");
        Self {
            canonical: bytes,
            frontier: (start, end),
        }
    }

    fn obligation_id(&self) -> Uuid {
        Uuid::new_v5(&JOURNAL_NAMESPACE, &self.canonical)
    }
    fn payload(&self) -> Value {
        json!({"canonical_key_sha256": format!("{:x}", Sha256::digest(&self.canonical))})
    }
}

fn push_field(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

fn execution_id(
    session: &str,
    generation: i64,
    epoch_snapshot: u64,
    user_message: u64,
    started_at: &str,
    start: Option<u64>,
) -> Uuid {
    Uuid::new_v5(
        &JOURNAL_NAMESPACE,
        format!(
            "watcher:{session}:{generation}:{epoch_snapshot}:{user_message}:{started_at}:{start:?}"
        )
        .as_bytes(),
    )
}

#[rustfmt::skip] #[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ShadowClassification { CandidateDelivered, SettledWithoutTransport, Unknown, ObservationGap }

/// Q3 classification for one obligation's shadow observation window.
#[rustfmt::skip]
pub(super) fn classify_shadow_observation(events: &[JournalEvent], grace_elapsed: bool) -> ShadowClassification {
    let Some(first) = events.first() else { return ShadowClassification::ObservationGap };
    if events.iter().any(|event| event.obligation_id != first.obligation_id) { return ShadowClassification::Unknown; }
    let find = |kind: &'static str| events.iter().find(|event| event.kind == kind);
    let count = |kind: &'static str| events.iter().filter(|event| event.kind == kind).count();
    let (o, a, t, c, s, u) = (find("O"), find("A"), find("T"), find("C"), find("S"), find("U"));
    if o.is_none() || count("O") != 1 { return ShadowClassification::ObservationGap; }
    if s.is_some() && a.is_none() && t.is_none() && c.is_none() && u.is_none() && count("S") == 1 { return ShadowClassification::SettledWithoutTransport; }
    let same_attempt = a.zip(c).is_some_and(|(a, c)| a.attempt_id.is_some() && a.attempt_id == c.attempt_id);
    let same_frontier = a.zip(c).is_some_and(|(a, c)| ["frontier_start", "frontier_end"].iter().all(|field| a.canonical_payload.get(*field).zip(c.canonical_payload.get(*field)).is_some_and(|(left, right)| !left.is_null() && left == right)));
    let receipt_confirmed = count("T") == 1 && t.and_then(|event| event.receipt.as_ref()).is_some_and(|r| !r.requested_channel_id.is_empty() && r.requested_channel_id == r.returned_channel_id && !r.message_id.is_empty());
    if a.is_some() && c.is_some() && count("A") == 1 && count("C") == 1 && s.is_none() && u.is_none() && same_attempt && same_frontier && receipt_confirmed { return ShadowClassification::CandidateDelivered; }
    if u.is_some() || (a.is_some() && s.is_none() && (t.is_some() || c.is_some() || grace_elapsed)) { return ShadowClassification::Unknown; }
    ShadowClassification::ObservationGap
}

fn source_coordinate(path: Option<&str>) -> (u64, Option<(u64, u64)>) {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.unwrap_or("").hash(&mut hasher);
    #[cfg(unix)]
    let file_id = path
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        });
    #[cfg(not(unix))]
    let file_id = None;
    (hasher.finish(), file_id)
}

fn encode_file_id(file_id: Option<(u64, u64)>, bytes: &mut Vec<u8>) {
    match file_id {
        None => bytes.push(0),
        Some((device, inode)) => {
            bytes.push(1);
            bytes.extend_from_slice(&device.to_be_bytes());
            bytes.extend_from_slice(&inode.to_be_bytes());
        }
    }
}

fn cohort_bucket(id: Uuid) -> u8 {
    id.as_bytes()[0] % 100
}

fn event(
    obligation_id: Uuid,
    attempt_id: Option<Uuid>,
    kind: &'static str,
    seq: i16,
    payload: Value,
) -> JournalEvent {
    let event_id = Uuid::new_v5(&obligation_id, format!("event:{kind}:{seq}").as_bytes());
    let mut key = Sha256::new();
    key.update(obligation_id.as_bytes());
    key.update(kind.as_bytes());
    key.update(seq.to_be_bytes());
    let idempotency_key = key.finalize().to_vec();
    JournalEvent {
        event_id,
        obligation_id,
        attempt_id,
        kind,
        seq,
        idempotency_key,
        canonical_payload: payload,
        receipt: None,
    }
}

fn transport_event(
    obligation_id: Uuid,
    attempt_id: Uuid,
    receipt: DiscordTransportReceipt,
) -> JournalEvent {
    let payload = json!({"requested_channel_id": receipt.requested_channel_id,
        "returned_channel_id": receipt.returned_channel_id, "message_id": receipt.message_id});
    let mut event = event(obligation_id, Some(attempt_id), "T", 2, payload);
    event.receipt = Some(receipt);
    event
}

#[cfg(test)]
mod sink_direct_semantics_tests {
    //! #5071 T1 S2 §7 T1-T6. These are RUNTIME semantic assertions: every test
    //! here calls the production function and inspects its value. None of them
    //! inspects source text, so a mutation that keeps the file compiling but
    //! changes what the code means still fails.

    use super::super::SessionBoundTerminalDeliveryRoute as Route;
    use super::*;

    fn lazy_test_pool() -> sqlx::PgPool {
        sqlx::Pool::<sqlx::Postgres>::connect_lazy("postgres://localhost/agentdesk_test")
            .expect("lazy test pool URL is valid")
    }

    fn fixture_attempt() -> AttemptObservation {
        AttemptObservation {
            obligation_id: Uuid::from_u128(7),
            attempt_id: Uuid::from_u128(8),
            frontier: (10, 20),
            pool: lazy_test_pool(),
        }
    }

    fn receipt(requested: u64, returned: u64, message_id: u64) -> DiscordTransportReceipt {
        DiscordTransportReceipt {
            requested_channel_id: requested.to_string(),
            returned_channel_id: returned.to_string(),
            message_id: message_id.to_string(),
        }
    }

    /// T1 (kills M1). The `else` arm of `deliver_response` delegates to
    /// `deliver_new_message_with_task_authority`, which owns the index-0
    /// obligation. If the sink-direct family also began one there the same
    /// delivery would be journalled twice.
    #[test]
    fn t1_new_message_delegation_is_not_a_sink_direct_obligation() {
        assert!(
            !journals_sink_direct(&Route::NewMessage, false),
            "NewMessage delegation belongs to the index-0 family; sink direct must not begin an obligation there"
        );
        assert!(
            !journals_sink_direct(&Route::NewMessage, true),
            "a NewMessage route stays out of the sink-direct family regardless of the cutover flag"
        );
    }

    /// T2 (kills M2). D2: the cutover short-replace branch returns before the
    /// sink-direct window opens and is instrumented by the controller family
    /// (S4), so observing it here would double-count that delivery.
    #[test]
    fn t2_cutover_short_replace_is_not_a_sink_direct_obligation() {
        assert!(
            !journals_sink_direct(&Route::PlaceholderEdit(MessageId::new(4_242)), true),
            "cutover short-replace is the controller family's obligation (S4), not sink direct"
        );
    }

    /// T3 (kills M3). Positive control: without it the predicate could decay to
    /// a constant `false` — i.e. the facade would never be called at all — and
    /// T1/T2 would still pass.
    #[test]
    fn t3_placeholder_edit_without_cutover_is_a_sink_direct_obligation() {
        assert!(
            journals_sink_direct(&Route::PlaceholderEdit(MessageId::new(4_242)), false),
            "the long-chunk and legacy-edit branches are exactly the sink direct family"
        );
    }

    /// T4 (kills M4). D4.3 anchor receipt rule: the journal's `T` must name the
    /// same message the legacy frontier committed, which is
    /// `message_ids.last()` on every multi-chunk sink path.
    #[test]
    fn t4_anchor_receipt_is_the_last_chunk_the_frontier_commits() {
        let receipts = vec![
            receipt(10, 10, 901),
            receipt(10, 10, 902),
            receipt(10, 10, 903),
        ];
        let anchor = anchor_receipt(&receipts).expect("a multi-chunk send has an anchor receipt");
        assert_eq!(
            anchor.message_id, "903",
            "T must point at the tail chunk (message_ids.last()), not the head"
        );
        assert_ne!(
            anchor.message_id, receipts[0].message_id,
            "anchoring on the head chunk would make T name a message the frontier did not commit"
        );
        assert_eq!(
            anchor_receipt(&receipts[..1])
                .expect("single chunk")
                .message_id,
            "901",
            "a single-chunk send anchors on its only message"
        );
        assert!(anchor_receipt(&[]).is_none(), "no send, no anchor receipt");
        assert_eq!(
            receipts
                .iter()
                .map(|r| r.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["901", "902", "903"],
            "receipt order is chunk delivery order"
        );
    }

    /// T5 (kills M5). The worst available forgery is journalling an uncommitted
    /// delivery as delivered. Only `Persisted` means the legacy frontier
    /// recorded the advance; the other two proofs must produce `U`.
    #[tokio::test]
    async fn t5_settle_derives_committed_from_the_frontier_proof() {
        let observer = JournalObserver::default();

        for stale in [
            SinkDeliveryProofResult::LandedStale,
            SinkDeliveryProofResult::LandedUnrecorded,
        ] {
            let mut attempt = Some(fixture_attempt());
            let events = settle(&observer, &mut attempt, Some(receipt(10, 10, 30)), stale);
            assert_eq!(
                events.len(),
                1,
                "{stale:?} is not a delivery: exactly one terminal event"
            );
            assert_eq!(
                events[0].kind, "U",
                "{stale:?} must be journalled as Unknown, never as T+C"
            );
            assert_eq!(
                events[0].canonical_payload["reason"],
                "commit_not_persisted"
            );
        }

        let mut attempt = Some(fixture_attempt());
        let events = settle(
            &observer,
            &mut attempt,
            Some(receipt(10, 10, 30)),
            SinkDeliveryProofResult::Persisted,
        );
        assert_eq!(
            events.iter().map(|event| event.kind).collect::<Vec<_>>(),
            vec!["T", "C"],
            "a persisted commit emits the transport and commit pair"
        );
        assert!(
            events[0].receipt.is_some(),
            "T carries the transport receipt"
        );

        // The mismatch detector must stay reachable: a receipt whose returned
        // channel differs from the requested one is Unknown even when the
        // legacy frontier persisted the advance.
        let mut attempt = Some(fixture_attempt());
        let events = settle(
            &observer,
            &mut attempt,
            Some(receipt(10, 20, 30)),
            SinkDeliveryProofResult::Persisted,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "U");
        assert_eq!(events[0].canonical_payload["reason"], "channel_mismatch");
    }

    /// T6 (kills M6). The observation is single-use. A second settle on the same
    /// delivery — a retry, or a future branch that settles twice — must not
    /// append a second terminal event, which the 0103 schema would reject.
    #[tokio::test]
    async fn t6_settle_consumes_the_attempt_and_never_settles_twice() {
        let observer = JournalObserver::default();
        let mut attempt = Some(fixture_attempt());

        assert_eq!(
            settle(
                &observer,
                &mut attempt,
                Some(receipt(10, 10, 30)),
                SinkDeliveryProofResult::Persisted
            )
            .len(),
            2,
            "the first settle emits T and C"
        );
        assert!(attempt.is_none(), "a settled observation must be consumed");
        assert!(
            settle(
                &observer,
                &mut attempt,
                Some(receipt(10, 10, 30)),
                SinkDeliveryProofResult::Persisted
            )
            .is_empty(),
            "a second settle on the same observation must emit nothing"
        );

        // A missing receipt is not a settlement: nothing is emitted and the
        // observation is left for shadow reconciliation to classify.
        let mut pending = Some(fixture_attempt());
        assert!(
            settle(
                &observer,
                &mut pending,
                None,
                SinkDeliveryProofResult::Persisted
            )
            .is_empty(),
            "no receipt, no terminal event"
        );
        assert!(
            pending.is_some(),
            "a missing receipt must not consume the observation"
        );

        assert!(
            settle(
                &observer,
                &mut None,
                Some(receipt(10, 10, 30)),
                SinkDeliveryProofResult::Persisted
            )
            .is_empty(),
            "an unobserved delivery settles into nothing"
        );
    }
}

#[rustfmt::skip]
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delivery_journal_defaults_to_legacy() {
        let runtime = crate::config::RuntimeSettingsConfig::default();
        assert_eq!(runtime.delivery_journal_mode, DeliveryJournalMode::Legacy);
        assert_eq!(runtime.delivery_journal_cohort_percent, 0);
        assert!(runtime.delivery_journal_internal_channel_ids.is_empty());
    }
    #[test]
    fn absent_file_id_has_one_canonical_sentinel_byte() {
        let (mut absent, mut present) = (vec![], vec![]);
        encode_file_id(None, &mut absent); encode_file_id(Some((0, 0)), &mut present);
        assert_eq!(absent, vec![0]);
        assert_eq!(present[0], 1); assert_ne!(absent, present);
    }
    #[test]
    fn execution_id_changes_for_each_canonical_component() {
        let base = execution_id("adk-claude-a", 77, 3, 42, "now", Some(10));
        assert_eq!(base, execution_id("adk-claude-a", 77, 3, 42, "now", Some(10)));
        for (name, other) in [
            ("session", execution_id("adk-claude-b", 77, 3, 42, "now", Some(10))),
            ("generation", execution_id("adk-claude-a", 78, 3, 42, "now", Some(10))),
            ("epoch", execution_id("adk-claude-a", 77, 4, 42, "now", Some(10))),
            ("user_message", execution_id("adk-claude-a", 77, 3, 43, "now", Some(10))),
            ("started_at", execution_id("adk-claude-a", 77, 3, 42, "later", Some(10))),
            ("start", execution_id("adk-claude-a", 77, 3, 42, "now", Some(11))),
        ] { assert_ne!(base, other, "{name} must participate in execution identity"); }
    }
    #[test]
    fn shadow_receipt_uses_returned_channel() {
        let receipt = DiscordTransportReceipt { requested_channel_id: "10".into(), returned_channel_id: "20".into(), message_id: "30".into() };
        let row = transport_event(Uuid::nil(), Uuid::nil(), receipt);
        assert_eq!(row.canonical_payload["returned_channel_id"], "20");
        assert_ne!(row.canonical_payload["returned_channel_id"], row.canonical_payload["requested_channel_id"]);
    }

    #[test]
    fn shadow_candidate_delivered_requires_transport_confirmed() {
        let obligation_id = Uuid::nil();
        let attempt_id = Uuid::from_u128(1);
        let mut events = vec![event(obligation_id, None, "O", 0, json!({"canonical_key_sha256":"fixture"})), event(obligation_id, Some(attempt_id), "A", 1, json!({"frontier_start":10,"frontier_end":20})), transport_event(obligation_id, attempt_id, DiscordTransportReceipt { requested_channel_id:"10".into(), returned_channel_id:"10".into(), message_id:"30".into() }), event(obligation_id, Some(attempt_id), "C", 3, json!({"frontier_start":10,"frontier_end":20}))];
        assert_eq!(classify_shadow_observation(&events, false), ShadowClassification::CandidateDelivered);
        events.retain(|event| event.kind != "T");
        assert_eq!(classify_shadow_observation(&events, false), ShadowClassification::Unknown, "a commit without transport confirmation is not a candidate");
        let settled = vec![event(obligation_id, None, "O", 0, json!({"canonical_key_sha256":"fixture"})), event(obligation_id, None, "S", 1, json!({"reason":"suppressed"}))];
        assert_eq!(classify_shadow_observation(&settled, false), ShadowClassification::SettledWithoutTransport);
    }

    #[tokio::test]
    async fn finish_fresh_emits_transport_and_commit_as_one_batch() {
        let observer = JournalObserver::default();
        let events = observer.finish_fresh(
            AttemptObservation {
                obligation_id: Uuid::from_u128(7),
                attempt_id: Uuid::from_u128(8),
                frontier: (10, 20),
                pool: sqlx::Pool::<sqlx::Postgres>::connect_lazy("postgres://localhost/agentdesk_test")
                    .expect("lazy test pool URL is valid"),
            },
            DiscordTransportReceipt {
                requested_channel_id: "10".into(),
                returned_channel_id: "10".into(),
                message_id: "30".into(),
            },
            true,
        );
        assert_eq!(events.len(), 2, "committed fresh delivery emits T and C");
        assert_eq!(events[0].kind, "T");
        assert!(events[0].receipt.is_some(), "T carries the transport receipt");
        assert_eq!(events[1].kind, "C");
        assert_eq!(events[1].seq, 3);
    }
}
