use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::DeliveryJournalMode;
use crate::services::discord::SharedData;
use crate::services::discord::outbound::DiscordTransportReceipt;

mod pg_store;

const JOURNAL_NAMESPACE: Uuid = Uuid::from_u128(0xd9829c0b_8692_4ef0_9396_f7d83aa84dd5);
const MAILBOX_CAPACITY: usize = 256;

#[derive(Clone)]
pub(super) struct JournalEvent {
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
