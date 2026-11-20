use super::*;
use crate::services::discord::queue_io::input_owner::effect::{self, Completion};

#[tokio::test(flavor = "current_thread")]
async fn readiness_review_expiry_keeps_the_late_ingress_member() {
    late_ingress(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn readiness_review_retry_never_reserves_the_late_ingress_member() {
    late_ingress(true).await;
}

async fn late_ingress(reserve_first: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    seed_generation();
    let channel = ChannelId::new(if reserve_first { 7_272_744 } else { 7_272_743 });
    let first = channel.get() * 1_000 + 1;
    let late = first + 1;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let (handle, _) = adopt(&shared, channel).await;
    let (_, _rest) = notices(channel.get()).await;
    shared.restart.reconcile_done.store(false, Relaxed);

    let mut a = live(channel, first, "held A");
    a.request.merge_consecutive = true;
    dispatch_text_intake(&deps, a).await.unwrap();
    let hold = handle
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let front = owner.front().ok_or("no front")?;
            owner.hold_readiness(&front, "idle_evidence_pending", 1_000)
        })
        .await
        .unwrap();
    let due = hold.due_at_ms;

    let (entered, admission_done) = tokio::sync::oneshot::channel();
    let (release, notice_ready) = tokio::sync::oneshot::channel();
    let mut b = live(channel, late, "later B");
    b.request.merge_consecutive = true;
    let dispatch = input_ingress::BEFORE_INGRESS_HOLD.scope(
        std::cell::RefCell::new(Some((entered, notice_ready))),
        dispatch_text_intake(&deps, b),
    );
    let race = async {
        admission_done
            .await
            .expect("B committed before its own ingress wake");
        assert_eq!(
            items(&handle)
                .await
                .iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>(),
            [first, late]
        );
        if reserve_first {
            let members = handle.v2_request(move |slot| {
                let owner = slot.as_mut().ok_or("no owner")?;
                let front = owner.front().ok_or("no front")?;
                owner.grant_front(&front)?;
                owner.claim_turn(first, &front).ok_or("no claim")?;
                let followers = owner.followers();
                let payload = serde_json::from_value(json!({
                    "frame":"prepared unit","digest":"unit","attribution":{},"followers":followers
                })).map_err(|e| e.to_string())?;
                let attempt = serde_json::from_value(json!({
                    "nonce":"late-retry","provider":"claude",
                    "target":{"host":"herdr","owner":"readiness-retry-fixture","root":"/test","execution_nonce":"e"},
                    "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}
                })).map_err(|e| e.to_string())?;
                owner.reserve_claimed((&front, owner.claim_number()), attempt, payload)?;
                owner.tick(due)?;
                let head = owner.snapshot.head_injection.as_ref().ok_or("reserved leaf lost its own attempt")?;
                Ok(head.members().map(|member| member.message_id).collect::<Vec<_>>())
            }).await.unwrap();
            assert_eq!(members, [first], "the retry absorbed the later original");
            let result = effect::run(
                &handle,
                Arc::new(move || due),
                Box::new(|_| {
                    crate::services::claude_tui::owner_input::vetoed("before payload");
                    Ok(Completion::Held("pre-key leaf".into()))
                }),
            )
            .await;
            assert!(matches!(result, Ok(Completion::NotSent(_))), "{result:?}");
        } else {
            assert!(
                handle
                    .v2_request(move |slot| {
                        slot.as_mut()
                            .ok_or("no owner")?
                            .refuse_readiness_front(&hold, due)
                    })
                    .await
                    .unwrap()
            );
        }
        let current = items(&handle).await;
        assert_eq!(
            current
                .iter()
                .map(|item| item.message_id)
                .collect::<Vec<_>>(),
            [late],
            "the earlier deadline removed the late input"
        );
        assert_eq!(current[0].text, "later B");
        release.send(()).unwrap();
    };
    let (admitted, ()) = tokio::join!(dispatch, race);
    admitted.unwrap();
    handle
        .v2_request(move |slot| {
            let owner = slot.as_ref().ok_or("no owner")?;
            assert_eq!(owner.front().unwrap().1.message_id, late);
            Ok(())
        })
        .await
        .unwrap();
}
