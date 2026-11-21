//! The first attempt of an approved Decision event against fake ports and a real PG row.
use super::super::{ExternalHold, InjectAttempt, InjectMode, MailboxHolder};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::decisions::EventKind;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

const TEXT: &str = "[사용자 결정 ref=r1] 질문 → 선택: A";

/// Scripted ports: each start answers the next scripted attempt, and every call is counted.
#[derive(Default)]
struct Ports {
    owner: bool,
    hold: Option<ExternalHold>,
    starts: Mutex<VecDeque<StartAttempt>>,
    enqueue_refusal: Option<&'static str>,
    started: AtomicUsize,
    enqueued: AtomicUsize,
}

#[async_trait]
impl DeliveryPorts for &Ports {
    async fn external_hold(&self) -> Option<ExternalHold> {
        self.hold
    }

    async fn try_start(&self) -> StartAttempt {
        self.started.fetch_add(1, SeqCst);
        self.starts.lock().unwrap().pop_front().expect("a start")
    }

    async fn mailbox_holder(&self) -> MailboxHolder {
        MailboxHolder::Nothing
    }

    async fn enqueue(&self) -> Result<String, String> {
        self.enqueued.fetch_add(1, SeqCst);
        match self.enqueue_refusal {
            Some(refusal) => Err(refusal.into()),
            None => Ok("discord:7:queued".into()),
        }
    }

    fn inject_mode(&self) -> InjectMode {
        InjectMode::All
    }

    async fn try_inject(&self) -> InjectAttempt {
        panic!("a decision never joins a running turn through the pane")
    }

    async fn owner_takes(&self) -> bool {
        self.owner
    }
}

fn ports(starts: impl IntoIterator<Item = StartAttempt>) -> Ports {
    Ports {
        starts: Mutex::new(starts.into_iter().collect()),
        ..Default::default()
    }
}

/// Counts normalizations and answers each with the next scripted report.
struct Normalizes(AtomicUsize, Mutex<VecDeque<Normalized>>);

#[async_trait]
impl Normalizer for Normalizes {
    async fn normalize_once(&self) -> Normalized {
        self.0.fetch_add(1, SeqCst);
        self.1.lock().unwrap().pop_front().expect("a normalization")
    }
}

fn normalizes(reports: impl IntoIterator<Item = Normalized>) -> Normalizes {
    Normalizes(
        AtomicUsize::new(0),
        Mutex::new(reports.into_iter().collect()),
    )
}

struct Pg {
    db: TestPostgresDb,
    pool: PgPool,
}

impl Pg {
    async fn new() -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        Self { db, pool }
    }

    async fn event(&self) -> Uuid {
        let event = (EventKind::Selection, 7, 200);
        crate::db::decisions::test_support::insert_event(&self.pool, event, TEXT, "not_started")
            .await
    }

    async fn attempt(&self, fake: &Ports, event: Uuid, normalizer: &Normalizes) -> FirstAttempt {
        let resolve = |_: &DecisionProvenance| async move { Ok(fake) };
        first_attempt(&self.pool, (event, TEXT), resolve, normalizer)
            .await
            .unwrap()
    }

    /// The workflow row: state, whether normalization was used, the stored ticket and turn.
    async fn row(&self, event: Uuid) -> (String, bool, Option<String>, Option<String>) {
        let row = crate::db::decisions::test_support::row(&self.pool, event).await;
        (row.0, row.1, row.2, row.3)
    }

    async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn started(turn: &str) -> StartAttempt {
    StartAttempt::Started(turn.into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decision_event_resubmitted_is_reserved_once_and_injected_once_pg() {
    let pg = Pg::new().await;
    let event = pg.event().await;
    let fake = ports([started("discord:7:1"), started("discord:7:2")]);
    let none = normalizes([]);
    let (first, second) = tokio::join!(
        pg.attempt(&fake, event, &none),
        pg.attempt(&fake, event, &none)
    );
    let third = pg.attempt(&fake, event, &none).await;
    let mut outcomes = [first, second, third];
    outcomes.sort_by_key(|outcome| *outcome == FirstAttempt::AlreadyReserved);
    let started = FirstAttempt::Started {
        turn_id: "discord:7:1".into(),
    };
    assert_eq!(
        outcomes,
        [
            started,
            FirstAttempt::AlreadyReserved,
            FirstAttempt::AlreadyReserved
        ]
    );
    assert_eq!(fake.started.load(SeqCst), 1, "one injection for one event");
    let (state, normalized, ticket, turn) = pg.row(event).await;
    assert_eq!((state.as_str(), normalized), ("may_have_sent", false));
    assert_eq!(turn.as_deref(), Some("discord:7:1"));
    // The ticket comes from the channel and the event origin alone, so a retry names the same one.
    let metadata = serde_json::json!({"human_input": {"origin_id": format!("decision:{event}")}});
    let identity = InputOrigin::decision_identity(7, Some("decision"), Some(&metadata)).unwrap();
    assert_eq!(ticket, Some(identity.1));
    pg.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_normal_busy_waits_as_one_original_pg() {
    let pg = Pg::new().await;
    let none = normalizes([]);
    // A turn another path owns holds the channel: the input is queued once and never started.
    let held = Ports {
        hold: Some(ExternalHold::Busy),
        ..ports([])
    };
    let event = pg.event().await;
    let queued = FirstAttempt::NormalWait {
        turn_id: "discord:7:queued".into(),
    };
    assert_eq!(pg.attempt(&held, event, &none).await, queued);
    assert_eq!(
        (held.started.load(SeqCst), held.enqueued.load(SeqCst)),
        (0, 1)
    );
    assert_eq!(pg.row(event).await.0, "normal_wait");
    // A start the mailbox refuses as busy sent nothing: the same single original waits.
    let busy = ports([StartAttempt::Busy]);
    let event = pg.event().await;
    assert_eq!(pg.attempt(&busy, event, &none).await, queued);
    assert_eq!(
        (busy.started.load(SeqCst), busy.enqueued.load(SeqCst)),
        (1, 1)
    );
    assert_eq!(pg.row(event).await.0, "normal_wait");
    // The input owner admits the original under the event's ticket and writes it later itself.
    let owner = Ports {
        owner: true,
        ..ports([StartAttempt::Accepted("discord:7:9".into())])
    };
    let event = pg.event().await;
    let accepted = FirstAttempt::NormalWait {
        turn_id: "discord:7:9".into(),
    };
    assert_eq!(pg.attempt(&owner, event, &none).await, accepted);
    assert_eq!(
        (owner.started.load(SeqCst), owner.enqueued.load(SeqCst)),
        (1, 0)
    );
    assert_eq!(pg.row(event).await.0, "normal_wait");
    assert_eq!(
        pg.attempt(&owner, event, &none).await,
        FirstAttempt::AlreadyReserved
    );
    assert_eq!(owner.started.load(SeqCst), 1);
    assert_eq!(none.0.load(SeqCst), 0);
    pg.close().await;
}

/// A start answers once its worker is spawned, so its answer is never the delivery: the row keeps
/// `may_have_sent` under the started turn, and nothing is started or delivered a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_start_answer_is_not_a_delivery_and_is_never_repeated_pg() {
    let pg = Pg::new().await;
    for (owner, answer) in [
        (false, started("discord:7:1")),
        (false, StartAttempt::Accepted("discord:7:1".into())),
        (true, started("discord:7:1")),
    ] {
        let idle = Ports {
            owner,
            ..ports([answer])
        };
        let event = pg.event().await;
        let outcome = pg.attempt(&idle, event, &normalizes([])).await;
        let turn_id = "discord:7:1".to_string();
        assert_eq!(outcome, FirstAttempt::Started { turn_id }, "owner={owner}");
        let (state, _, _, turn) = pg.row(event).await;
        assert_eq!(state, "may_have_sent", "a start alone proves no delivery");
        assert_eq!(turn.as_deref(), Some("discord:7:1"));
        // A provider that failed before its first payload leaves this row as it is; the event
        // is already past its one start, so a later call neither starts nor delivers again.
        assert_eq!(
            pg.attempt(&idle, event, &normalizes([])).await,
            FirstAttempt::AlreadyReserved
        );
        assert_eq!(
            (idle.started.load(SeqCst), idle.enqueued.load(SeqCst)),
            (1, 0)
        );
    }
    // A normalization whose session start carried the input is the same kind of answer.
    let absent = ports([StartAttempt::Unavailable("no session".into())]);
    let event = pg.event().await;
    let normalizer = normalizes([Normalized::FirstPrompt("discord:7:5".into())]);
    let turn_id = "discord:7:5".to_string();
    assert_eq!(
        pg.attempt(&absent, event, &normalizer).await,
        FirstAttempt::Started { turn_id }
    );
    assert_eq!(
        (absent.started.load(SeqCst), absent.enqueued.load(SeqCst)),
        (1, 0),
        "no deliver after the normalization carried the input"
    );
    let (state, normalized, _, turn) = pg.row(event).await;
    assert_eq!((state.as_str(), normalized), ("may_have_sent", true));
    assert_eq!(turn.as_deref(), Some("discord:7:5"));
    pg.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_only_a_confirmed_unsent_refusal_normalizes_and_only_once_pg() {
    let pg = Pg::new().await;
    // Not sent, normalized, started on the session that normalization brought up.
    let recovers = ports([
        StartAttempt::Unavailable("no session".into()),
        started("discord:7:2"),
    ]);
    let event = pg.event().await;
    let normalizer = normalizes([Normalized::Started]);
    let started_after = FirstAttempt::Started {
        turn_id: "discord:7:2".into(),
    };
    assert_eq!(
        pg.attempt(&recovers, event, &normalizer).await,
        started_after
    );
    assert_eq!(
        (recovers.started.load(SeqCst), normalizer.0.load(SeqCst)),
        (2, 1)
    );
    // Not sent again after the one normalization: closed as failed, with no third start.
    let refuses = Ports {
        enqueue_refusal: Some("full"),
        hold: Some(ExternalHold::Busy),
        ..ports([])
    };
    let event = pg.event().await;
    let normalizer = normalizes([Normalized::Started]);
    let failed = FirstAttempt::Failed("queue_refused: full".into());
    assert_eq!(pg.attempt(&refuses, event, &normalizer).await, failed);
    assert_eq!(
        (refuses.enqueued.load(SeqCst), normalizer.0.load(SeqCst)),
        (2, 1)
    );
    let (state, normalized, _, _) = pg.row(event).await;
    assert_eq!((state.as_str(), normalized), ("failed", true));
    // A normalization that cannot say whether it sent ends unknown and is never sent again.
    let absent = ports([StartAttempt::Unavailable("no session".into())]);
    let event = pg.event().await;
    let normalizer = normalizes([Normalized::Unknown("start timed out".into())]);
    let unknown = FirstAttempt::Unknown("start timed out".into());
    assert_eq!(pg.attempt(&absent, event, &normalizer).await, unknown);
    assert_eq!(pg.row(event).await.0, "may_have_sent");
    assert_eq!(
        pg.attempt(&absent, event, &normalizer).await,
        FirstAttempt::AlreadyReserved
    );
    assert_eq!(absent.started.load(SeqCst), 1);
    // Refusals that are no proof of an unsent input end as failed without any normalization.
    let unproven = [
        (true, StartAttempt::Busy),
        (true, StartAttempt::Unavailable("owner error".into())),
        (false, StartAttempt::InvalidTarget("no such channel".into())),
    ];
    for (owner, refusal) in unproven {
        let fake = Ports {
            owner,
            ..ports([refusal])
        };
        let event = pg.event().await;
        let normalizer = normalizes([]);
        let outcome = pg.attempt(&fake, event, &normalizer).await;
        assert!(matches!(outcome, FirstAttempt::Failed(_)), "{outcome:?}");
        assert_eq!(
            (fake.started.load(SeqCst), normalizer.0.load(SeqCst)),
            (1, 0)
        );
        let (state, normalized, _, _) = pg.row(event).await;
        assert_eq!((state.as_str(), normalized), ("failed", false));
    }
    pg.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decision_unapproved_event_or_text_reserves_and_sends_nothing_pg() {
    let pg = Pg::new().await;
    let fake = ports([]);
    let none = normalizes([]);
    let event = pg.event().await;
    let resolve = |_: &DecisionProvenance| async { Ok(&fake) };
    for (id, text) in [(event, "다른 문장"), (Uuid::new_v4(), TEXT)] {
        let outcome = first_attempt(&pg.pool, (id, text), resolve, &none).await;
        assert_eq!(outcome.unwrap(), FirstAttempt::NotApproved);
    }
    assert_eq!(pg.row(event).await.0, "not_started");
    assert_eq!(fake.started.load(SeqCst) + fake.enqueued.load(SeqCst), 0);
    // A runtime that no longer exists is the absent session: one normalization, then closed.
    let gone = |_: &DecisionProvenance| async {
        Err::<&Ports, _>(HumanInputError::RuntimeUnavailable("no runtime".into()))
    };
    let normalizer = normalizes([Normalized::NotSentRefused("start refused".into())]);
    let outcome = first_attempt(&pg.pool, (event, TEXT), gone, &normalizer).await;
    assert_eq!(
        outcome.unwrap(),
        FirstAttempt::Failed("start refused".into())
    );
    assert_eq!(normalizer.0.load(SeqCst), 1);
    // An actor the channel does not allow is refused outright.
    let event = pg.event().await;
    let denied =
        |_: &DecisionProvenance| async { Err::<&Ports, _>(HumanInputError::AuthorNotAllowed) };
    let outcome = first_attempt(&pg.pool, (event, TEXT), denied, &none).await;
    assert_eq!(
        outcome.unwrap(),
        FirstAttempt::Failed("author_not_allowed".into())
    );
    assert_eq!(none.0.load(SeqCst), 0);
    pg.close().await;
}
