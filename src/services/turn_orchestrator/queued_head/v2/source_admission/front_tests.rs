//! The owner's front and its grant: the front names the ticket bound to its own slot, and only an
//! owner claim presenting that exact front spends the grant, once.
use super::*;
use crate::config::TestRuntimeRootGuard;

fn s3b() -> crate::services::turn_orchestrator::QueuePersistenceContext {
    let claude = crate::services::provider::ProviderKind::Claude;
    crate::services::turn_orchestrator::QueuePersistenceContext::new(&claude, "s3b", None)
}

async fn front(handle: &Handle) -> Option<super::super::super::Front> {
    handle
        .v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.front()))
        .await
        .unwrap()
}

/// Grants `grant` if given, then answers each claim for `id` behind the queue, in one actor turn.
async fn claims(
    handle: &Handle,
    id: u64,
    grant: Option<super::super::super::Front>,
    claims: Vec<Option<super::super::super::Front>>,
) -> Vec<bool> {
    use crate::services::turn_orchestrator::TurnAdmissionOrder::BehindQueue;
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            if let Some(grant) = &grant {
                owner.grant_front(grant)?;
            }
            Ok((claims.iter())
                .map(|claim| owner.blocks_claim(id, claim.as_ref(), BehindQueue))
                .collect())
        })
        .await
        .unwrap()
}

/// The front names its own slot's ticket, never a settled admission of the same body across a
/// restart; a migrated or partly admitted front still runs with no ticket.
#[tokio::test]
async fn the_owner_front_names_the_ticket_bound_to_its_own_slot() {
    let _root = TestRuntimeRootGuard::new();
    let ticket = |raw: &str| Some(raw.to_string());
    let channel = ChannelId::new(727_640);
    let owned = ticket_owner(channel.get(), "tickets").await;
    let (id, body) = (8_000_000_000_000_001_640_u64, "same body");
    for run in ["run-a", "run-b", "run-c"] {
        let admitted = owned.1.v2_admit_ticket(original(id, body), run.into());
        assert_eq!(admitted.await, Ok(Admitted::Queued));
    }
    assert_eq!(front(&owned.1).await.unwrap().0, ticket("run-a"));
    let cancelled = owned
        .1
        .cancel_queued_primary_message(poise::serenity_prelude::MessageId::new(id), s3b());
    assert!(cancelled.await.removed.is_some());
    assert_eq!(front(&owned.1).await.unwrap().0, ticket("run-b"));
    owned.1.v2_arm_head_for_tests("attempt-b").await.unwrap();
    assert!(
        front(&owned.1).await.is_none(),
        "an attempted head is no front"
    );
    settle_head(&owned.1).await;
    assert_eq!(front(&owned.1).await.unwrap().0, ticket("run-c"));
    drop(owned);
    let (_registry, handle) = readopted(channel).await;
    let after = front(&handle).await.unwrap();
    assert_eq!((after.0, after.1.text), (ticket("run-c"), body.into()));

    let migrated = ChannelId::new(727_641);
    let legacy = crate::services::turn_orchestrator::queued_head::snapshot_json(
        vec![original(id, "legacy A")],
        None,
    )
    .unwrap();
    std::fs::create_dir_all(fixture_path(migrated.get()).parent().unwrap()).unwrap();
    std::fs::write(fixture_path(migrated.get()), legacy).unwrap();
    let (_registry, handle) = ticket_owner(migrated.get(), "tickets").await;
    let found = front(&handle).await.expect("a migrated front runs");
    assert_eq!((found.0, found.1.text), (None, "legacy A".into()));

    let (_registry, handle) = ticket_owner(727_642, "tickets").await;
    let (a, b) = (727_642_100_u64, 727_642_200_u64);
    let known = handle.v2_admit_ticket(original(a, "known A"), "run-a".into());
    assert_eq!(known.await, Ok(Admitted::Queued));
    handle.v2_arm_head_for_tests("attempt-a").await.unwrap();
    settle_head(&handle).await;
    let mut whole = original(a, "known A\nfresh B");
    whole.source_message_ids = vec![a, b];
    whole.source_text_segments = serde_json::from_value(json!([
        {"message_id":a,"text":"known A"},{"message_id":b,"text":"fresh B"}]))
    .unwrap();
    let partial = handle.v2_admit_ticket(whole.clone(), "run-ab".into());
    assert_eq!(partial.await, Ok(Admitted::Queued));
    let part = front(&handle).await.expect("a partly admitted front runs");
    assert_eq!((part.0.clone(), part.1.text.as_str()), (None, "fresh B"));
    let id = part.1.message_id;
    assert_eq!(
        claims(&handle, id, Some(part.clone()), vec![Some(part)]).await,
        [false]
    );
    handle.v2_arm_head_for_tests("attempt-b").await.unwrap();
    settle_head(&handle).await;
    assert_eq!(turn(&handle, &whole, "run-ab").await, Ok(TurnBind::Unknown));
}

/// Only an owner claim presenting the exact granted front spends the grant, once: a bare message ID,
/// another ticket or another body never does, and a cancel or clear revokes it.
#[tokio::test]
async fn an_owner_grant_is_spent_once_and_only_by_its_exact_front() {
    let _root = TestRuntimeRootGuard::new();
    let (_registry, handle) = ticket_owner(727_643, "tickets").await;
    let id = 8_000_000_000_000_001_643_u64;
    for (run, body) in [("run-a", "body A"), ("run-b", "body B")] {
        let admitted = handle.v2_admit_ticket(original(id, body), run.into());
        assert_eq!(admitted.await, Ok(Admitted::Queued));
    }
    let a = front(&handle).await.unwrap();
    let mut other_ticket = a.clone();
    other_ticket.0 = Some("run-b".into());
    let mut other_body = a.clone();
    other_body.1 = original(id, "body B");
    let spent = claims(
        &handle,
        id,
        Some(a.clone()),
        vec![Some(other_ticket), Some(a.clone())],
    );
    assert_eq!(spent.await, [true, true], "another ticket spends nothing");
    let spent = claims(
        &handle,
        id,
        Some(a.clone()),
        vec![Some(other_body), Some(a.clone())],
    );
    assert_eq!(spent.await, [true, true], "another body spends nothing");
    let spent = claims(
        &handle,
        id,
        Some(a.clone()),
        vec![None, Some(a.clone()), Some(a.clone())],
    );
    assert_eq!(
        spent.await,
        [true, false, true],
        "a message ID never; the exact front once"
    );

    assert_eq!(
        claims(&handle, id, Some(a.clone()), vec![]).await,
        Vec::<bool>::new()
    );
    let cancelled =
        handle.cancel_queued_primary_message(poise::serenity_prelude::MessageId::new(id), s3b());
    assert!(cancelled.await.removed.is_some());
    let b = front(&handle).await.unwrap();
    assert_eq!(b.1.text, "body B");
    let stale = claims(&handle, id, None, vec![Some(b.clone()), Some(a.clone())]);
    assert_eq!(stale.await, [true, true], "A's grant never starts B");
    assert_eq!(
        claims(&handle, id, Some(b.clone()), vec![Some(b)]).await,
        [false]
    );

    // Two identical untracked fronts: only the revocation tells the second from the first.
    let twins = ChannelId::new(727_644);
    let legacy = original(id, "twin");
    let raw = crate::services::turn_orchestrator::queued_head::snapshot_json(
        vec![legacy.clone(), legacy],
        None,
    )
    .unwrap();
    std::fs::create_dir_all(fixture_path(twins.get()).parent().unwrap()).unwrap();
    std::fs::write(fixture_path(twins.get()), raw).unwrap();
    let (_registry, handle) = ticket_owner(twins.get(), "tickets").await;
    let first = front(&handle).await.unwrap();
    assert_eq!(
        claims(&handle, id, Some(first.clone()), vec![]).await,
        Vec::<bool>::new()
    );
    let cancelled =
        handle.cancel_queued_primary_message(poise::serenity_prelude::MessageId::new(id), s3b());
    assert!(cancelled.await.removed.is_some());
    assert_eq!(
        claims(&handle, id, None, vec![Some(first.clone())]).await,
        [true]
    );
    let again = claims(&handle, id, Some(first.clone()), vec![]).await;
    assert!(again.is_empty());
    assert!(handle.clear(s3b()).await.persistence.failed.is_empty());
    assert!(front(&handle).await.is_none());
    let admitted = handle.v2_admit_ticket(original(id, "twin"), "run-t".into());
    assert_eq!(admitted.await, Ok(Admitted::Queued));
    let cleared = claims(&handle, id, None, vec![Some(first.clone())]).await;
    assert_eq!(
        cleared,
        [true],
        "a clear revokes the grant before the next admission"
    );
}
