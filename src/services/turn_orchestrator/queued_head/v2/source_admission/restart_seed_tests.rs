use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

const SEED: u64 = (1 << 63) | 8128;

fn person(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":8128,
        "source_message_ids":[id],"queued_generation":5,"text":format!("waiting {id}")}))
    .unwrap()
}

fn seed(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":1,"author_is_bot":true,"message_id":id,
        "channel_id":8128,"source_message_ids":[id],"queued_generation":5,
        "text":"안녕하세요. 세션 재시작 완료."}))
    .unwrap()
}

fn context(operation: &str) -> serde_json::Value {
    json!({"origin":"headless","source":SOURCE,"request_owner":1,
        "metadata":{"restart_seed":{"operation":operation,"target_nonce":"old","dispatch":null}}})
}

fn owner(channel: u64) -> Owner {
    let path = super::super::super::super::pending_queue_file_path(
        &ProviderKind::Claude,
        "restart-seed",
        ChannelId::new(channel),
    )
    .unwrap();
    Owner::adopt(path, format!("scope-{channel}")).unwrap()
}

fn admit(owner: &mut Owner, id: u64, ticket: &str) -> Result<Admitted, AdmitError> {
    owner.admit_with_context(&seed(id), Ticket::new(ticket).unwrap(), context(ticket))
}

fn waiting(owner: &mut Owner, id: u64) {
    let ticket = Ticket::new(&format!("person-{id}")).unwrap();
    let admitted = Custody::with_ticket(&person(id), ticket).advance(owner);
    assert_eq!(admitted, Ok(Admitted::Queued));
}

fn order(owner: &Owner) -> Vec<u64> {
    owner.snapshot.items.iter().map(|i| i.message_id).collect()
}

fn attempt() -> AttemptIdentity {
    AttemptIdentity {
        nonce: "nonce".into(),
        provider: "claude".into(),
        target: TargetIdentity {
            host: "herdr".into(),
            owner: "owner".into(),
            root: "/test".into(),
            execution_nonce: "old".into(),
        },
        source: AttemptSource::PreSource {
            expected_session_id: None,
            expected_transcript: None,
            binding_watermark: None,
        },
    }
}

fn payload() -> PreparedPayload {
    PreparedPayload {
        frame: "frame".into(),
        digest: "digest".into(),
        attribution: json!({}),
        followers: Vec::new(),
    }
}

#[test]
fn restart_seed_is_durable_after_a_reserved_unit_and_ahead_of_waiting_input() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(81281);
    waiting(&mut owner, 1);
    waiting(&mut owner, 2);
    owner.reserve(attempt(), payload()).unwrap();
    assert_eq!(admit(&mut owner, SEED, "restart-a"), Ok(Admitted::Queued));
    assert_eq!(
        order(&owner),
        [1, SEED, 2],
        "behind the unit in flight only"
    );
    let head = owner.snapshot.head_injection.clone().unwrap();
    assert!(
        super::super::super::front_holds(&owner.snapshot.items, &head),
        "the unit in flight stays the queue front so its settlement finds it"
    );
    // Every waiting ticket still names its own original after the move.
    let behind = Ticket::new("person-2").unwrap();
    assert_eq!(
        owner.exact_turn(&behind, &person(2)),
        Ok(TurnBind::NotArmed)
    );
    assert_eq!(
        owner.seed_status(&Ticket::new("restart-a").unwrap()),
        SeedStatus::Queued
    );
    // The commit is the acceptance: a new process reads the same order, ticket and context.
    let restored = self::owner(81281);
    assert_eq!(order(&restored), [1, SEED, 2]);
    assert_eq!(
        restored.seed_status(&Ticket::new("restart-a").unwrap()),
        SeedStatus::Queued
    );
    let json = restored.json().unwrap();
    let kept = &json["admission_binds"];
    assert!(
        kept.as_array().unwrap().iter().any(|bind| {
            bind["at"]["kind"] == "queued"
                && bind["at"]["index"] == 1
                && bind["context"]["execution"] == context("restart-a")
        }),
        "{kept}"
    );
}

#[test]
fn restart_seed_becomes_the_front_and_no_person_overtakes_it() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(81282);
    waiting(&mut owner, 1);
    waiting(&mut owner, 2);
    let before = owner.front().unwrap();
    owner.grant_front(&before).unwrap();
    assert_eq!(admit(&mut owner, SEED, "restart-a"), Ok(Admitted::Queued));
    assert_eq!(order(&owner), [SEED, 1, 2]);
    assert!(
        !owner.front_outstanding(),
        "the earlier front's grant is withdrawn with its place"
    );
    let front = owner.front().unwrap();
    assert_eq!(front.0.as_deref(), Some("restart-a"));
    assert_eq!(owner.prefer_person(&front), Ok(false));
    assert_eq!(order(&owner), [SEED, 1, 2], "the greeting runs first");
    assert!(owner.followers().is_empty(), "a greeting merges nobody");
}

#[test]
fn restart_seed_follows_a_claimed_front_and_its_fixed_followers() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(81283);
    waiting(&mut owner, 1);
    let front = owner.front().unwrap();
    owner.grant_front(&front).unwrap();
    let claim = owner.claim_turn(1, &front).unwrap();
    assert_eq!(admit(&mut owner, SEED, "restart-a"), Ok(Admitted::Queued));
    waiting(&mut owner, 2);
    assert_eq!(order(&owner), [1, SEED, 2]);
    assert!(owner.claimed(1, &claim), "the claim keeps its front");
}

#[test]
fn restart_seed_takes_the_spare_slot_of_a_full_queue_and_a_second_restart_joins_it() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(81284);
    for id in 1..=30 {
        waiting(&mut owner, id);
    }
    assert_eq!(admit(&mut owner, SEED, "restart-a"), Ok(Admitted::Queued));
    assert_eq!(owner.snapshot.items.len(), 31);
    assert_eq!(order(&owner)[0], SEED);
    // Another operation reuses the queued greeting: a receipt and a bind, no second item.
    assert_eq!(
        admit(&mut owner, SEED + 1, "restart-b"),
        Ok(Admitted::Duplicate)
    );
    assert_eq!(owner.snapshot.items.len(), 31);
    for ticket in ["restart-a", "restart-b"] {
        assert_eq!(
            owner.seed_status(&Ticket::new(ticket).unwrap()),
            SeedStatus::Queued,
            "{ticket}"
        );
    }
    assert_eq!(
        admit(&mut owner, SEED + 1, "restart-b"),
        Err(AdmitError::Conflict(
            "ticket original or execution context differs".into()
        )),
        "a joined ticket is answered from its status, never admitted again"
    );
    let restored = self::owner(81284);
    assert_eq!(
        restored.seed_status(&Ticket::new("restart-b").unwrap()),
        SeedStatus::Queued
    );
    assert_eq!(
        restored.seed_status(&Ticket::new("restart-c").unwrap()),
        SeedStatus::Absent
    );
}

#[test]
fn restart_seed_withdrawal_takes_only_a_queued_greeting_and_owes_no_origin() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(81285);
    waiting(&mut owner, 1);
    assert_eq!(admit(&mut owner, SEED, "restart-a"), Ok(Admitted::Queued));
    let ticket = Ticket::new("restart-a").unwrap();
    assert_eq!(owner.withdraw_seed(&ticket), Ok(true));
    assert_eq!(order(&owner), [1], "the waiting original keeps its place");
    assert_eq!(owner.seed_status(&ticket), SeedStatus::Gone);
    assert!(owner.owed_drops().is_empty(), "a greeting names no origin");
    assert_eq!(owner.withdraw_seed(&ticket), Ok(false));
    let waiting = Ticket::new("person-1").unwrap();
    assert_eq!(
        owner.exact_turn(&waiting, &person(1)),
        Ok(TurnBind::NotArmed)
    );
    assert!(
        matches!(
            admit(&mut owner, SEED, "restart-a"),
            Err(AdmitError::Conflict(_))
        ),
        "a withdrawn greeting is never admitted again under its ticket"
    );
    assert_eq!(order(&owner), [1]);
}

#[test]
fn restart_seed_snapshot_keeps_the_old_format() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(81286);
    waiting(&mut owner, 1);
    let plain: Vec<String> = (owner.json().unwrap().as_object().unwrap().keys())
        .cloned()
        .collect();
    assert_eq!(admit(&mut owner, SEED, "restart-a"), Ok(Admitted::Queued));
    let json = owner.json().unwrap();
    let seeded: Vec<String> = json.as_object().unwrap().keys().cloned().collect();
    assert_eq!(seeded, plain, "no new snapshot field carries the greeting");
    assert_eq!(json["version"], 2);
    // An older binary reads the greeting's bind as an ordinary headless start.
    let execution = &json["admission_binds"][1]["context"]["execution"];
    assert_eq!(execution["origin"], "headless");
    assert!(!ChannelMailboxHandle::person_input(Some(execution)));
    assert!(!ChannelMailboxHandle::effort_input(Some(execution)));
}
