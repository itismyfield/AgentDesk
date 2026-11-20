use std::cell::{Cell, RefCell};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use super::plan::{Seal, Terminal};
use super::*;
use crate::domain::message_split::{DISCORD_MSG_LIMIT, discord_message_units};
use crate::services::tui_o::store::{Initialized, OEra, OStore, StoreConfig, fault};
use crate::services::tui_o::writer::DeliveryLease;
use uuid::Uuid;

const fn id(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

struct Fixture {
    root: tempfile::TempDir,
    store: OStore,
    era: OEra,
    key: TurnKey,
    lease: TurnLease,
    gate: OwnershipGate,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
            .unwrap()
            .unwrap();
        let now = chrono::Utc::now();
        let era = store
            .begin_fixture_era(&[7], now, |channel| {
                Ok(Initialized {
                    channel,
                    at: now,
                    sources: vec![],
                    initial_anchor: 0,
                    build_digest: "test".into(),
                    policy_parent: None,
                })
            })
            .unwrap();
        let key = TurnKey {
            destination: id(7),
            id: id(100),
            nonce: Uuid::new_v4(),
        };
        let lease = TurnLease {
            key,
            revision: id(1),
        };
        let gate = OwnershipGate::default();
        gate.acquired();
        Self {
            root,
            store,
            era,
            key,
            lease,
            gate,
        }
    }

    fn epoch(&self) -> u64 {
        self.gate.current().epoch().unwrap()
    }

    fn open_store(&self) -> ChannelStore {
        self.store.open_channel(&self.era, 7).unwrap().unwrap()
    }

    fn plan(&self, body: &str) -> Plan {
        Plan::derive(Seal {
            key: self.key,
            last_seq: 8,
            terminal: Terminal::Done {
                conversation: self.key.nonce,
                body: body.into(),
                sha256: hash(body),
                split_version: 1,
            },
        })
        .unwrap()
    }

    fn dir(&self) -> PathBuf {
        self.root.path().join("o_store/7/agy-delivery")
    }
}

#[derive(Default)]
struct Wire {
    messages: Mutex<Vec<Evidence>>,
    calls: std::sync::atomic::AtomicUsize,
    next: Mutex<Option<Post>>,
    after_created: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    blocked: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl Transport for Wire {
    fn bot(&self) -> NonZeroU64 {
        id(9)
    }

    async fn post(&self, channel: u64, piece: &Piece) -> Post {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let outcome = self.next.lock().unwrap().take();
        if let Some(Post::NotSent(_)) = outcome {
            return outcome.unwrap();
        }
        let message = {
            let mut wire = self.messages.lock().unwrap();
            let message = id(1000 + wire.len() as u64);
            wire.push(Evidence {
                id: message,
                channel,
                bot: self.bot().get(),
                content: piece.payload.clone(),
                correlation: Some(piece.correlation.clone()),
            });
            message
        };
        if let Some(notice) = self.after_created.lock().unwrap().take() {
            notice.send(()).unwrap();
        }
        let block = self.blocked.lock().unwrap().take();
        if let Some(block) = block {
            let _ = block.await;
        }
        outcome.unwrap_or(Post::Created(message))
    }

    async fn history_before(
        &self,
        _channel: u64,
        before: Option<NonZeroU64>,
    ) -> Result<Vec<Evidence>, String> {
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .rev()
            .filter(|item| before.is_none_or(|id| item.id < id))
            .take(crate::services::tui_o::writer::confirm::HISTORY_PAGE)
            .map(|item| Evidence {
                id: item.id,
                channel: item.channel,
                bot: item.bot,
                content: item.content.clone(),
                correlation: item.correlation.clone(),
            })
            .collect())
    }
}

struct Delivery;

impl DeliveryLease for Delivery {
    type Held = ();
    fn try_acquire(&self, _channel: u64, _serial: u64) -> Option<()> {
        Some(())
    }
}

#[derive(Default)]
struct Effects {
    inflight: Cell<Option<TurnLease>>,
    active: Cell<Option<TurnLease>>,
    queued: Cell<bool>,
    releases: Cell<usize>,
    claims: Cell<usize>,
    calls: RefCell<Vec<Effect>>,
}

impl Projection for Effects {
    fn apply(
        &self,
        lease: TurnLease,
        effect: Effect,
    ) -> impl Future<Output = Result<(), String>> + Send {
        self.calls.borrow_mut().push(effect);
        match effect {
            Effect::ClearInflight if self.inflight.get() == Some(lease) => self.inflight.set(None),
            Effect::ReleaseLease if self.active.get() == Some(lease) => {
                self.active.set(None);
                self.releases.set(self.releases.get() + 1);
            }
            Effect::WakeQueue if self.active.get().is_none() && self.queued.replace(false) => {
                self.claims.set(self.claims.get() + 1);
            }
            _ => {}
        }
        std::future::ready(Ok(()))
    }
}

async fn finish(replay: &mut Replay<'_>, wire: &Wire, effects: &Effects) {
    for _ in 0..30 {
        match replay.advance(wire, &Delivery, effects).await.unwrap() {
            Progress::More => {}
            Progress::Complete => return,
            other => panic!("unexpected {other:?}"),
        }
    }
    panic!("delivery did not finish");
}

#[test]
fn canonical_split_retains_line_breaks_and_fences_within_utf16_budget() {
    let fixture = Fixture::new();
    let body = format!(
        "{}\n  ```rust\r\n{}\n```\n\n ",
        "첫".repeat(1700),
        "😀".repeat(1200)
    );
    let plan = fixture.plan(&body);
    let mut offset = 0;
    let mut joined = String::new();
    for piece in &plan.body {
        assert_eq!(piece.source.start, offset);
        assert!(discord_message_units(&piece.payload) <= DISCORD_MSG_LIMIT);
        let source = &body[piece.source.clone()];
        assert!(piece.payload.contains(source));
        joined.push_str(source);
        offset = piece.source.end;
    }
    assert_eq!(joined, body);
    assert!(
        plan.body
            .iter()
            .any(|piece| piece.payload.starts_with("```rust\n"))
    );
}

#[tokio::test]
async fn repeated_done_keeps_two_receipts_and_posts_only_the_suffix() {
    let fixture = Fixture::new();
    let piece = format!("{}{}\n", "a".repeat(798), "😀".repeat(582));
    let original = format!("{piece}{piece}{}", "b".repeat(1366));
    assert_eq!(original.len(), 7620);
    let plan = fixture.plan(&original);
    let wire = Wire::default();
    let effects = Effects::default();
    effects.active.set(Some(fixture.lease));
    effects.inflight.set(Some(fixture.lease));
    effects.queued.set(true);
    let mut store = fixture.open_store();
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    for _ in 0..4 {
        assert_eq!(
            replay.advance(&wire, &Delivery, &effects).await.unwrap(),
            Progress::More
        );
    }
    let offset = replay.confirmed_bytes();
    assert_eq!(offset, plan.body[1].source.end);
    assert_eq!(offset, 6254);
    drop(replay);
    let mut store = fixture.open_store();
    let conflict = fixture.plan("another result from the same child");
    assert!(matches!(
        Replay::open(
            &mut store,
            &fixture.gate,
            fixture.epoch(),
            fixture.lease,
            &conflict
        ),
        Err(Error::Store(_))
    ));
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    assert_eq!(replay.confirmed_bytes(), offset);
    finish(&mut replay, &wire, &effects).await;
    finish(&mut replay, &wire, &effects).await;
    assert_eq!(replay.confirmed_bytes(), original.len());
    let messages = wire.messages.lock().unwrap();
    assert_eq!(messages.len(), plan.body.len() + 1);
    assert_eq!((messages[0].id, messages[1].id), (id(1000), id(1001)));
    assert_eq!(
        messages[..plan.body.len()]
            .iter()
            .map(|m| m.content.as_str())
            .collect::<String>(),
        original
    );
    assert_eq!(effects.releases.get(), 1);
    assert_eq!(effects.claims.get(), 1);
    assert_eq!(effects.inflight.get(), None);
}

#[tokio::test]
async fn receipt_write_rejection_recovers_the_exact_post() {
    let fixture = Fixture::new();
    let plan = fixture.plan("short answer");
    let wire = Wire::default();
    let effects = Effects::default();
    let mut store = fixture.open_store();
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    replay.advance(&wire, &Delivery, &effects).await.unwrap();
    let (created, made) = tokio::sync::oneshot::channel();
    let (resume, blocked) = tokio::sync::oneshot::channel();
    *wire.after_created.lock().unwrap() = Some(created);
    *wire.blocked.lock().unwrap() = Some(blocked);
    let (result, _) = tokio::join!(replay.advance(&wire, &Delivery, &effects), async {
        made.await.unwrap();
        let fault = fault::plant(
            &fixture.dir(),
            fault::Step::Write,
            io::ErrorKind::StorageFull,
            Some(1),
        );
        resume.send(()).unwrap();
        fault
    });
    assert!(matches!(result, Err(Error::Store(_))));
    assert_eq!(replay.confirmed_bytes(), 0);
    drop(replay);
    let mut store = fixture.open_store();
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    finish(&mut replay, &wire, &effects).await;
    assert_eq!(wire.messages.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn lost_owner_cannot_ack_or_post_after_reacquisition() {
    let fixture = Fixture::new();
    let plan = fixture.plan("short answer");
    let wire = Wire::default();
    let effects = Effects::default();
    let mut store = fixture.open_store();
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    replay.advance(&wire, &Delivery, &effects).await.unwrap();
    let (created, made) = tokio::sync::oneshot::channel();
    let (_resume, blocked) = tokio::sync::oneshot::channel();
    *wire.after_created.lock().unwrap() = Some(created);
    *wire.blocked.lock().unwrap() = Some(blocked);
    let (result, _) = tokio::join!(replay.advance(&wire, &Delivery, &effects), async {
        made.await.unwrap();
        fixture.gate.lost();
        fixture.gate.acquired();
    });
    assert!(matches!(result, Err(Error::Authority)));
    assert!(matches!(
        replay.advance(&wire, &Delivery, &effects).await,
        Err(Error::Authority)
    ));
    drop(replay);
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    finish(&mut replay, &wire, &effects).await;
    assert_eq!(wire.messages.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn end_cas_and_projection_resume_across_both_write_boundaries() {
    let fixture = Fixture::new();
    let plan = fixture.plan("short answer");
    let wire = Wire::default();
    let effects = Effects::default();
    effects.inflight.set(Some(fixture.lease));
    effects.active.set(Some(fixture.lease));
    effects.queued.set(true);
    let mut store = fixture.open_store();
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    replay.advance(&wire, &Delivery, &effects).await.unwrap();
    replay.advance(&wire, &Delivery, &effects).await.unwrap();
    let fault = fault::plant(
        &fixture.dir(),
        fault::Step::ReplaceDirSync,
        io::ErrorKind::StorageFull,
        Some(1),
    );
    assert!(matches!(
        replay.advance(&wire, &Delivery, &effects).await,
        Err(Error::Store(_))
    ));
    assert!(effects.calls.borrow().is_empty());
    drop(fault);
    drop(replay);

    let mut store = fixture.open_store();
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    assert_eq!(replay.journal.state().ended, Some(Stage::Footer));
    for _ in 0..3 {
        replay.advance(&wire, &Delivery, &effects).await.unwrap();
    }
    assert_eq!(replay.journal.state().ended, Some(Stage::Release));
    let fault = fault::plant(
        &fixture.dir(),
        fault::Step::Write,
        io::ErrorKind::StorageFull,
        Some(1),
    );
    assert!(matches!(
        replay.advance(&wire, &Delivery, &effects).await,
        Err(Error::Store(_))
    ));
    assert_eq!(effects.releases.get(), 1);
    assert_eq!(effects.claims.get(), 0);
    drop(fault);
    drop(replay);
    fixture.gate.lost();
    fixture.gate.acquired();

    let mut store = fixture.open_store();
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    finish(&mut replay, &wire, &effects).await;
    assert_eq!(effects.releases.get(), 1);
    assert_eq!(effects.claims.get(), 1);
    assert_eq!(wire.messages.lock().unwrap().len(), 2);
    assert_eq!(
        &*effects.calls.borrow(),
        &[
            Effect::ClearInflight,
            Effect::ReleaseLease,
            Effect::ReleaseLease,
            Effect::WakeQueue
        ]
    );
}

#[tokio::test]
async fn prepared_waits_for_positive_evidence_and_retries_only_proven_not_sent() {
    let fixture = Fixture::new();
    let plan = fixture.plan("short answer");
    let wire = Wire::default();
    let effects = Effects::default();
    let mut store = fixture.open_store();
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    replay.advance(&wire, &Delivery, &effects).await.unwrap();
    *wire.next.lock().unwrap() = Some(Post::NotSent("permission".into()));
    assert_eq!(
        replay.advance(&wire, &Delivery, &effects).await.unwrap(),
        Progress::Held("permission".into())
    );
    *wire.next.lock().unwrap() = Some(Post::Unknown);
    assert!(matches!(
        replay.advance(&wire, &Delivery, &effects).await.unwrap(),
        Progress::Held(_)
    ));
    let correlation = wire.messages.lock().unwrap()[0].correlation.take();
    assert!(matches!(
        replay.advance(&wire, &Delivery, &effects).await.unwrap(),
        Progress::Held(_)
    ));
    assert_eq!(replay.confirmed_bytes(), 0);
    assert_eq!(wire.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    wire.messages.lock().unwrap()[0].correlation = correlation;
    finish(&mut replay, &wire, &effects).await;
    assert_eq!(wire.messages.lock().unwrap().len(), 2);
}

#[test]
fn lease_revision_and_concurrent_writer_cannot_reopen_the_same_turn() {
    let fixture = Fixture::new();
    let plan = fixture.plan("short answer");
    let mut store = fixture.open_store();
    let mut concurrent = fixture.open_store();
    let mut replay = Replay::open(
        &mut store,
        &fixture.gate,
        fixture.epoch(),
        fixture.lease,
        &plan,
    )
    .unwrap();
    assert!(matches!(
        Replay::open(
            &mut concurrent,
            &fixture.gate,
            fixture.epoch(),
            fixture.lease,
            &plan
        ),
        Err(Error::Store(_))
    ));
    replay
        .commit(|state| state.attempt = Some(Attempt::Prepared))
        .unwrap();
    drop(replay);
    let lease = TurnLease {
        revision: id(2),
        ..fixture.lease
    };
    assert!(matches!(
        Replay::open(
            &mut concurrent,
            &fixture.gate,
            fixture.epoch(),
            lease,
            &plan
        ),
        Err(Error::Store(_))
    ));
}
