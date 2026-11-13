use super::*;

async fn claim(mailbox: &ChannelMailboxHandle) -> FrontClaim {
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner missing")?;
            let front = owner.front().ok_or("front missing")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    claim_turn(mailbox, front.1.message_id, &front)
        .await
        .unwrap()
        .unwrap()
}

async fn arrive(mailbox: &ChannelMailboxHandle, channel: u64, n: u64) {
    assert!(matches!(
        admit(mailbox, &original(channel, n), AdmissionTicket::new()).await,
        super::super::super::admission::Admission::Accepted(_)
    ));
}

async fn submit_claim(
    mailbox: &ChannelMailboxHandle,
    channel: u64,
    claim: FrontClaim,
) -> Result<Completion, String> {
    let text = std::iter::once(&claim.front().1)
        .chain(claim.followers())
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let expected = text.clone();
    let leaf: Leaf = Box::new(move |frame| {
        assert_eq!(
            frame, expected,
            "the frame must contain only claimed originals"
        );
        Ok(Completion::Submitted)
    });
    pump::submit_front(
        mailbox,
        Some(claim),
        prepared(channel, &text),
        Arc::new(|| 10_000),
        leaf,
    )
    .await
}

#[tokio::test]
async fn claim_followers_are_fixed_after_same_author_arrival() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_781;
    let mailbox = owner(&registry, channel, 2).await;
    let claim = claim(&mailbox).await;
    assert_eq!(
        claim.followers().len(),
        1,
        "the fixture must claim a merged unit"
    );
    let expected = original(channel, 1).message_id;
    assert_eq!(claim.followers()[0].message_id, expected);
    arrive(&mailbox, channel, 2).await;
    let followers = mailbox
        .v2_request(|slot| {
            Ok(slot
                .as_ref()
                .ok_or("owner missing")?
                .followers()
                .into_iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>())
        })
        .await
        .unwrap();
    assert_eq!(
        followers,
        [expected],
        "arrival cannot enlarge the claimed unit"
    );
    assert_eq!(
        submit_claim(&mailbox, channel, claim).await,
        Ok(Completion::Submitted)
    );
    let after = snapshot(&mailbox).await;
    assert_eq!(after["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        after["items"][0]["message_id"],
        original(channel, 2).message_id
    );
    assert_eq!(after["terminal_identities"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn claimed_reserve_refuses_a_later_arrival_as_a_follower() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_782;
    let mailbox = owner(&registry, channel, 2).await;
    let claim = claim(&mailbox).await;
    arrive(&mailbox, channel, 2).await;
    let before = snapshot(&mailbox).await;
    for followers in [vec![], vec![original(channel, 1), original(channel, 2)]] {
        let (front, number) = (claim.front().clone(), claim.number());
        let attempt = prepared(channel, "altered unit").attempt;
        let refused = mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("owner missing")?;
                let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
                let payload = serde_json::from_value(json!({"frame":"altered unit",
                    "digest":"digest","attribution":{},"followers":followers}))
                .map_err(|e| e.to_string())?;
                owner
                    .reserve_claimed((&front, number), attempt, payload)
                    .map(drop)
            })
            .await;
        assert!(refused.is_err(), "a claim cannot omit or add an original");
        assert_eq!(
            snapshot(&mailbox).await,
            before,
            "refusal changes no custody"
        );
    }
    assert_eq!(
        submit_claim(&mailbox, channel, claim).await,
        Ok(Completion::Submitted),
        "a refused altered payload leaves the real claim usable"
    );
}

#[tokio::test]
async fn cancelling_a_late_arrival_preserves_the_claimed_unit() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_783;
    let mailbox = owner(&registry, channel, 2).await;
    let claim = claim(&mailbox).await;
    arrive(&mailbox, channel, 2).await;
    let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        &crate::services::provider::ProviderKind::Claude,
        "drive",
        None,
    );
    let removed = mailbox
        .cancel_queued_primary_message(
            poise::serenity_prelude::MessageId::new(original(channel, 2).message_id),
            persistence,
        )
        .await;
    assert!(removed.persistence_error.is_none());
    assert_eq!(
        removed.removed.unwrap().message_id.get(),
        original(channel, 2).message_id
    );
    assert_eq!(
        submit_claim(&mailbox, channel, claim).await,
        Ok(Completion::Submitted),
        "cancelling an arrival outside the claimed unit cannot revoke that claim"
    );
    assert!(
        snapshot(&mailbox).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
