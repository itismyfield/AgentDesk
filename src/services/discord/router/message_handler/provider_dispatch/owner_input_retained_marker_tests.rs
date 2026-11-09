use super::*;

#[tokio::test]
async fn retained_host_refusal_clears_only_its_queued_generation() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::retained_markers::",
        "retained_host_refusal_clears_only_its_queued_generation"
    )) {
        return;
    }
    retained_refusal_marker_case(false).await;
}

#[tokio::test]
async fn retained_host_refusal_preserves_a_newer_source_generation() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::retained_markers::",
        "retained_host_refusal_preserves_a_newer_source_generation"
    )) {
        return;
    }
    retained_refusal_marker_case(true).await;
}

async fn retained_refusal_marker_case(newer: bool) {
    use crate::services::discord::turn_view_reconciler as view;
    use crate::test_support::wait::{WAIT_CAP, await_within};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_853_101, false).await;
    let front = rig
        .mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner")?;
            let front = owner.front().ok_or("front")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let mut waiting = front.1.clone();
    waiting.message_id += 1;
    waiting.source_message_ids = vec![waiting.message_id];
    waiting.text = "another live queued input".into();
    waiting.merge_consecutive = false;
    assert!(matches!(
        admit(&rig.mailbox, &waiting, AdmissionTicket::new()).await,
        Admission::Accepted(_)
    ));
    let http = &rig.shared.http.cached_serenity_ctx.get().unwrap().http;
    for item in [&front.1, &waiting] {
        assert!(
            view::note_intake_queue_marker_added(
                &rig.shared,
                http,
                rig.channel,
                poise::serenity_prelude::MessageId::new(item.message_id),
                item.queued_generation + u64::from(newer && item.message_id == front.1.message_id),
                '📬',
                "test_queue_seed",
            )
            .await
        );
    }
    // The configured remote endpoint is a real pre-start refusal, before any database lookup.
    let _hosts = crate::config::session_hosts::force_for_test(
        Some("local-fixture"),
        &[(rig.channel.get(), "remote-fixture")],
    );
    let result = await_within(
        super::super::super::start_idle(rig.shared.clone(), rig.channel, front.clone()),
        WAIT_CAP,
    )
    .await;
    assert_eq!(result, Err("NotSent(\"owner_start_failed\")".into()));
    let state = rig.state().await;
    assert_eq!(
        state["retained_inputs"].as_array().unwrap().len(),
        1,
        "{state}"
    );
    assert_eq!(
        state["retained_inputs"][0]["input"]["original"]["message_id"],
        front.1.message_id
    );
    assert_eq!(state["items"].as_array().unwrap().len(), 1, "{state}");
    assert_eq!(state["items"][0]["message_id"], waiting.message_id);
    assert!(events(&rig.renderer).is_empty(), "refusal sent no input");
    let present = |message| {
        rig.shared
            .turn_view_reconciler
            .ops()
            .iter()
            .filter(|op| op.target.message_id.get() == message && op.emoji == '📬')
            .fold(false, |_, op| op.add)
    };
    assert!(
        present(waiting.message_id),
        "another queued source keeps its marker"
    );
    assert_eq!(
        present(front.1.message_id),
        newer,
        "only the retained source generation is cleared"
    );
}
