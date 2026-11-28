use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

use chrono::{DateTime, TimeDelta, Utc};
use uuid::Uuid;

use super::{eligibility::*, schedules::pending_schedule};
use crate::{
    config::{RuntimeSettingsConfig, TestRuntimeRootGuard},
    db::{auto_queue::test_support::TestPostgresDb, transcript_rotation_requests},
    services::{
        tui_o::{channel_policy::Adoption, writer::actor::Undelivered},
        turn_orchestrator::queued_head::{
            v2::{
                Fault, Owner,
                rotation::{Handoff, HandoffError, sha256},
            },
            v2_types::{
                previous_v2,
                rotation::{RotationIntent, Ticket},
            },
        },
    },
};

fn clock() -> DateTime<Utc> {
    DateTime::from_timestamp_millis(1_800_000_000_000).unwrap()
}

fn eligibility(debt: &Undelivered) -> Eligibility<'_> {
    Eligibility {
        transcript_bytes: 42,
        adoption: Adoption::Committed,
        undelivered: Some(debt),
        has_blocker: false,
        queue_empty: true,
        capability: Capability {
            cooperative_exit: true,
            fresh_exact: true,
            exit_queues_behind_turn: true,
        },
        origin: InputOrigin::Ordinary,
    }
}

struct QueueFixture(TestRuntimeRootGuard);

impl QueueFixture {
    fn new() -> Self {
        let root = TestRuntimeRootGuard::new();
        std::fs::create_dir(root.path().join("runtime")).unwrap();
        Self(root)
    }

    fn path(&self, name: impl AsRef<Path>) -> PathBuf {
        self.0.path().join("runtime/rotation-tests").join(name)
    }
}

fn owner(path: &Path) -> Owner {
    Owner::adopt(path.to_path_buf(), "claude/rotation-tests".into()).unwrap()
}

fn request(native_session_id: Uuid) -> (RotationIntent, Handoff) {
    let op_id = Uuid::new_v4();
    let ticket = Uuid::new_v4();
    let intent = RotationIntent {
        op_id,
        native_session_id,
        session_row_id: 1,
        requested_at: clock(),
        ticket: Ticket {
            digest: sha256(ticket.as_bytes()),
            expires_at: clock() + TimeDelta::minutes(7),
            revoked: false,
        },
        submission: None,
    };
    let handoff = Handoff {
        op_id,
        native_session_id,
        ticket,
        body: "First body\n".into(),
    };
    (intent, handoff)
}

#[tokio::test]
async fn rotation_latch_insert_before_owner_commit_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let fixture = QueueFixture::new();
    let path = fixture.path("queue.json");
    let mut owner = owner(&path);
    let initial = std::fs::read(&path).unwrap();
    let native = Uuid::new_v4();
    let (intent, _) = request(native);
    let op = intent.op_id;
    owner.fault = Fault::Before;
    assert!(
        owner
            .reserve_rotation(
                Some(42),
                &eligibility(&Undelivered::default()),
                intent,
                || false,
                || Some((&pool, "claude", "7".into()))
            )
            .await
            .is_err()
    );
    assert!(owner.snapshot.rotation.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), initial);
    let saved = transcript_rotation_requests::get(&pool, "claude", native)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.op_id, op);
    assert_eq!(saved.channel_id, "7");
    assert_eq!(saved.requested_at, clock());
    owner.fault = Fault::None;
    assert!(
        !owner
            .reserve_rotation(
                Some(42),
                &eligibility(&Undelivered::default()),
                request(native).0,
                || false,
                || Some((&pool, "claude", "7".into()))
            )
            .await
            .unwrap()
    );
    assert_eq!(std::fs::read(&path).unwrap(), initial);
}

#[tokio::test]
async fn rotation_latch_survives_reselect_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let fixture = QueueFixture::new();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    for (index, (native, expected)) in [(a, true), (b, true), (a, false)].into_iter().enumerate() {
        let mut relocated = owner(&fixture.path(format!("{index}.json")));
        assert_eq!(
            relocated
                .reserve_rotation(
                    Some(42),
                    &eligibility(&Undelivered::default()),
                    request(native).0,
                    || false,
                    || Some((&pool, "claude", index.to_string()))
                )
                .await
                .unwrap(),
            expected
        );
    }
    let fresh = Uuid::new_v4();
    let first = transcript_rotation_requests::Request {
        op_id: Uuid::new_v4(),
        channel_id: "7".into(),
        requested_at: clock(),
    };
    let second = transcript_rotation_requests::Request {
        op_id: Uuid::new_v4(),
        channel_id: "8".into(),
        requested_at: clock(),
    };
    let (a, b) = tokio::join!(
        transcript_rotation_requests::insert(&pool, "claude", fresh, &first),
        transcript_rotation_requests::insert(&pool, "claude", fresh, &second)
    );
    let winner = if a.unwrap() { &first } else { &second };
    assert_eq!(
        transcript_rotation_requests::get(&pool, "claude", fresh)
            .await
            .unwrap()
            .as_ref(),
        Some(winner)
    );
    assert_eq!(b.unwrap(), winner == &second);
    assert!(
        transcript_rotation_requests::insert(&pool, "codex", fresh, &first)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn rotation_ineligible_never_touches_latch() {
    let fixture = QueueFixture::new();
    let path = fixture.path("queue.json");
    let mut owner = owner(&path);
    let initial = std::fs::read(&path).unwrap();
    let zero = Undelivered::default();
    let debt = Undelivered {
        posting: 1,
        ..Undelivered::default()
    };
    let cases = [
        Eligibility {
            transcript_bytes: 41,
            ..eligibility(&zero)
        },
        Eligibility {
            adoption: Adoption::Held,
            ..eligibility(&zero)
        },
        Eligibility {
            undelivered: None,
            ..eligibility(&zero)
        },
        eligibility(&debt),
        Eligibility {
            has_blocker: true,
            ..eligibility(&zero)
        },
        Eligibility {
            queue_empty: false,
            ..eligibility(&zero)
        },
        Eligibility {
            capability: Capability {
                fresh_exact: false,
                ..eligibility(&zero).capability
            },
            ..eligibility(&zero)
        },
        Eligibility {
            capability: Capability {
                cooperative_exit: false,
                ..eligibility(&zero).capability
            },
            ..eligibility(&zero)
        },
        Eligibility {
            capability: Capability {
                exit_queues_behind_turn: false,
                ..eligibility(&zero).capability
            },
            ..eligibility(&zero)
        },
    ];
    assert!(eligible(42, &eligibility(&zero)));
    for evidence in cases {
        assert!(
            !owner
                .reserve_rotation(
                    Some(42),
                    &evidence,
                    request(Uuid::new_v4()).0,
                    || panic!("ineligible request read a transcript"),
                    || panic!("ineligible request opened PG")
                )
                .await
                .unwrap()
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), initial);
}

#[tokio::test]
async fn rotation_pending_schedule_blocks() {
    // Captured from Claude Code 2.1.325 during a one-shot schedule probe.
    let rows = include_str!("../../../tests/fixtures/transcript_rotation/cron-one-shot.jsonl");
    let (before, _) = rows.trim_end().rsplit_once('\n').unwrap();
    let native = Uuid::parse_str("708eaac7-d46f-4c29-837a-84e79c2d811f").unwrap();
    assert!(pending_schedule(Cursor::new(before), native));
    assert!(!pending_schedule(Cursor::new(rows), native));
    assert!(!pending_schedule(Cursor::new(rows), Uuid::new_v4()));
    assert!(pending_schedule(Cursor::new(&b"{broken\n"[..]), native));
    assert!(pending_schedule(
        Cursor::new(rows.lines().next().unwrap()),
        native
    ));
    let recurring = rows.replace(r#""recurring":false"#, r#""recurring":true"#);
    assert!(pending_schedule(Cursor::new(recurring), native));

    let fixture = QueueFixture::new();
    let path = fixture.path("queue.json");
    let mut owner = owner(&path);
    let initial = std::fs::read(&path).unwrap();
    assert!(
        !owner
            .reserve_rotation(
                Some(42),
                &eligibility(&Undelivered::default()),
                request(native).0,
                || pending_schedule(Cursor::new(before), native),
                || panic!("pending schedule opened PG")
            )
            .await
            .unwrap()
    );
    assert_eq!(std::fs::read(&path).unwrap(), initial);
}

#[tokio::test]
async fn rotation_seed_turn_never_triggers() {
    let fixture = QueueFixture::new();
    let path = fixture.path("queue.json");
    let mut owner = owner(&path);
    let initial = std::fs::read(&path).unwrap();
    let zero = Undelivered::default();
    for origin in [InputOrigin::RotationSeed, InputOrigin::RotationRequest] {
        assert!(
            !owner
                .reserve_rotation(
                    Some(42),
                    &Eligibility {
                        origin,
                        ..eligibility(&zero)
                    },
                    request(Uuid::new_v4()).0,
                    || panic!("rotation turn opened the transcript"),
                    || panic!("rotation turn spent a fresh UUID")
                )
                .await
                .unwrap()
        );
    }
    assert_eq!(std::fs::read(&path).unwrap(), initial);
}

#[tokio::test]
async fn rotation_disabled_by_default_spends_nothing() {
    let config: RuntimeSettingsConfig = serde_yaml::from_str("{}").unwrap();
    assert!(config.is_empty());
    assert!(
        serde_json::to_value(&config)
            .unwrap()
            .get("transcript_rotation_soft_bytes")
            .is_none()
    );
    let fixture = QueueFixture::new();
    let path = fixture.path("queue.json");
    let mut owner = owner(&path);
    let initial = std::fs::read(&path).unwrap();
    assert!(
        !owner
            .reserve_rotation(
                config.transcript_rotation_soft_bytes,
                &eligibility(&Undelivered::default()),
                request(Uuid::new_v4()).0,
                || panic!("off opened the transcript"),
                || panic!("off opened PG")
            )
            .await
            .unwrap()
    );
    assert_eq!(std::fs::read(&path).unwrap(), initial);
}

#[test]
fn rotation_receipt_replay_precedes_gone() {
    let fixture = QueueFixture::new();
    let path = fixture.path("queue.json");
    let (intent, mut submitted) = request(Uuid::new_v4());
    let now = intent.ticket.expires_at - TimeDelta::nanoseconds(1);
    let due = intent.ticket.expires_at;
    let mut active = owner(&path);
    active.snapshot.rotation = Some(intent);
    let saved = active.stage_rotation(1, copy(&submitted), now).unwrap();
    let mut restarted = owner(&path);
    restarted.snapshot.rotation.as_mut().unwrap().ticket.revoked = true;
    assert_eq!(
        restarted.stage_rotation(1, copy(&submitted), due).unwrap(),
        saved
    );
    submitted.body.push(' ');
    assert_eq!(
        restarted.stage_rotation(1, submitted, due),
        Err(HandoffError::Conflict)
    );
    let (mut intent, new) = request(Uuid::new_v4());
    restarted.snapshot.rotation = Some(intent.clone());
    assert_eq!(
        restarted.stage_rotation(1, copy(&new), due),
        Err(HandoffError::Gone)
    );
    intent.ticket.revoked = true;
    restarted.snapshot.rotation = Some(intent);
    assert_eq!(
        restarted.stage_rotation(1, new, now),
        Err(HandoffError::Gone)
    );
}

fn copy(handoff: &Handoff) -> Handoff {
    serde_json::from_value(serde_json::to_value(handoff).unwrap()).unwrap()
}

#[test]
fn rotation_ticket_single_use_and_bound() {
    let fixture = QueueFixture::new();
    let path = fixture.path("queue.json");
    let (intent, good) = request(Uuid::new_v4());
    let mut owner = owner(&path);
    let initial = std::fs::read(&path).unwrap();
    owner.snapshot.rotation = Some(intent);
    for (row, bad) in [
        (2, copy(&good)),
        (
            1,
            Handoff {
                op_id: Uuid::new_v4(),
                ..copy(&good)
            },
        ),
        (
            1,
            Handoff {
                native_session_id: Uuid::new_v4(),
                ..copy(&good)
            },
        ),
        (
            1,
            Handoff {
                ticket: Uuid::new_v4(),
                ..copy(&good)
            },
        ),
    ] {
        assert_eq!(
            owner.stage_rotation(row, bad, clock()),
            Err(HandoffError::Conflict)
        );
        assert_eq!(std::fs::read(&path).unwrap(), initial);
    }
    let first = owner.stage_rotation(1, copy(&good), clock()).unwrap();
    let original = std::fs::read(&path).unwrap();
    assert_eq!(
        owner
            .stage_rotation(1, copy(&good), clock() + TimeDelta::seconds(2))
            .unwrap(),
        first
    );
    assert_eq!(
        owner.stage_rotation(
            1,
            Handoff {
                body: "second body".into(),
                ..good
            },
            clock()
        ),
        Err(HandoffError::Conflict)
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn rotation_previous_owner_reader_discards_optional_field() {
    let fixture = QueueFixture::new();
    let path = fixture.path("queue.json");
    let mut pending = owner(&path);
    let (intent, handoff) = request(Uuid::new_v4());
    assert!(pending.json().unwrap().get("rotation").is_none());
    pending.snapshot.rotation = Some(intent);
    pending.stage_rotation(1, handoff, clock()).unwrap();
    let staged = std::fs::read(&path).unwrap();
    let old: previous_v2::SnapshotV2 = serde_json::from_slice(&staged).unwrap();
    assert_eq!(old.version, 2);
    assert_eq!(old.namespace.as_deref(), Some("claude/rotation-tests"));
    assert!(old.items.is_empty());
    let old_bytes = serde_json::to_vec(&old).unwrap();
    assert!(
        serde_json::from_slice::<serde_json::Value>(&old_bytes)
            .unwrap()
            .get("rotation")
            .is_none()
    );
    std::fs::write(&path, old_bytes).unwrap();
    assert!(owner(&path).snapshot.rotation.is_none());
}
