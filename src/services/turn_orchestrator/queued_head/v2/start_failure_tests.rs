use super::*;
use serde_json::json;

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":id,"message_id":id,"channel_id":7787991,"text":format!("raw {id}"),"queued_generation":1})).unwrap()
}
fn claim(owner: &mut Owner) -> (Front, u64) {
    let front = owner.front().unwrap();
    owner.grant_front(&front).unwrap();
    owner.claim_turn(front.1.message_id, &front).unwrap();
    (front, owner.claim_number())
}
fn queue_dir() -> tempfile::TempDir {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn owner(path: std::path::PathBuf) -> Owner {
    let mut owner = Owner::adopt(path, "start-failure".into()).unwrap();
    owner.enqueue_fixture(&item(7787901)).unwrap();
    owner.enqueue_fixture(&item(7787902)).unwrap();
    owner
}
#[test]
fn first_failure_retries_second_retains_and_pin_cas_wakes_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let temp = queue_dir();
    let mut owner = owner(temp.path().join("q.json"));
    let pin = json!({"birth":"first"});
    let (front, n) = claim(&mut owner);
    assert_eq!(
        owner
            .start_failed(
                &front,
                n,
                std::slice::from_ref(&front.1),
                Some(pin.clone()),
                StartFailureKind::Opaque,
                "first"
            )
            .unwrap(),
        Some(false)
    );
    assert!(owner.front().is_none());
    assert!(owner.snapshot.retained_inputs.is_empty());
    assert_eq!(
        owner
            .start_failed(
                &front,
                n,
                std::slice::from_ref(&front.1),
                Some(pin.clone()),
                StartFailureKind::Opaque,
                "duplicate"
            )
            .unwrap(),
        None
    );
    assert!(!owner.start_cleanup_completed(&json!({"birth":"other"})));
    assert!(owner.start_cleanup_completed(&pin));
    assert!(!owner.start_cleanup_completed(&pin));
    let (front, n) = claim(&mut owner);
    assert_eq!(
        owner
            .start_failed(
                &front,
                n,
                std::slice::from_ref(&front.1),
                Some(pin.clone()),
                StartFailureKind::Opaque,
                "different second error"
            )
            .unwrap(),
        Some(true)
    );
    assert!(owner.front().is_none());
    assert!(owner.start_cleanup_completed(&pin));
    assert_eq!(owner.front().unwrap().1.message_id, 7787902);
    let retained = &owner.snapshot.retained_inputs[0];
    assert!(retained.input.attempt.is_none());
    assert!(matches!(
        retained.disposition,
        Some(RetainedDisposition::KnownNotSent)
    ));
    assert!(owner.requeue_known_not_sent().unwrap().is_empty());
    assert_eq!(owner.snapshot.retained_inputs.len(), 1);
}
#[test]
fn successful_reserve_resets_failures_and_cannot_be_reclassified() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let temp = queue_dir();
    let mut owner = owner(temp.path().join("q.json"));
    let (front, n) = claim(&mut owner);
    owner
        .start_failed(
            &front,
            n,
            std::slice::from_ref(&front.1),
            None,
            StartFailureKind::Opaque,
            "before reserve",
        )
        .unwrap();
    let (front, n) = claim(&mut owner);
    let attempt=serde_json::from_value(json!({"nonce":"after-failure","provider":"claude","target":{"host":"tmux","owner":"fixture","root":"fixture","execution_nonce":"born"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})).unwrap();
    let payload = PreparedPayload {
        frame: "frame".into(),
        digest: "digest".into(),
        attribution: json!({}),
        followers: vec![],
    };
    owner
        .reserve_claimed((&front, n), attempt, payload)
        .unwrap();
    assert!(
        owner.starts.failures.values().all(|n| *n == 0),
        "reserve must reset before effect settlement"
    );
    assert_eq!(
        owner
            .start_failed(
                &front,
                n,
                std::slice::from_ref(&front.1),
                None,
                StartFailureKind::ExecutorNotWired,
                "NotSent(text cannot override reserve)"
            )
            .unwrap(),
        None
    );
    assert!(owner.snapshot.retained_inputs.is_empty());
}
#[test]
fn confirmed_kind_retains_first_and_attemptless_notice_ack_survives_adoption() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let temp = queue_dir();
    let path = temp.path().join("q.json");
    let mut owner = owner(path.clone());
    let (front, n) = claim(&mut owner);
    assert_eq!(
        owner
            .start_failed(
                &front,
                n,
                std::slice::from_ref(&front.1),
                None,
                StartFailureKind::EndpointNotLocal,
                "typed refusal"
            )
            .unwrap(),
        Some(true)
    );
    let identity = owner.snapshot.retained_inputs[0].input.identity.clone();
    owner.attemptless_notice_enqueued(identity).unwrap();
    let mut adopted = Owner::adopt(path, "start-failure".into()).unwrap();
    assert!(adopted.snapshot.retained_inputs[0].notice.enqueued);
    assert!(adopted.requeue_known_not_sent().unwrap().is_empty());
    assert_eq!(adopted.front().unwrap().1.message_id, 7787902);
}
#[test]
fn adoption_resets_runtime_count_and_old_claim_cannot_settle_new_claim() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let temp = queue_dir();
    let path = temp.path().join("q.json");
    let mut first = owner(path.clone());
    let (front, old) = claim(&mut first);
    first
        .start_failed(
            &front,
            old,
            std::slice::from_ref(&front.1),
            None,
            StartFailureKind::Opaque,
            "old process",
        )
        .unwrap();
    let mut adopted = Owner::adopt(path, "start-failure".into()).unwrap();
    let (front, new) = claim(&mut adopted);
    assert_ne!(old, new);
    assert_eq!(
        adopted
            .start_failed(
                &front,
                old,
                std::slice::from_ref(&front.1),
                None,
                StartFailureKind::Opaque,
                "late"
            )
            .unwrap(),
        None
    );
    assert_eq!(
        adopted
            .start_failed(
                &front,
                new,
                std::slice::from_ref(&front.1),
                None,
                StartFailureKind::Opaque,
                "first after boot"
            )
            .unwrap(),
        Some(false)
    );
    assert!(adopted.snapshot.retained_inputs.is_empty());
}
