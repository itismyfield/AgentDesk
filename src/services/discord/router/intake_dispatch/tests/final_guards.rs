use super::*;

#[tokio::test(flavor = "current_thread")]
async fn skill_dispatch_rejects_a_mismatched_local_permit() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(6_325_727);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("test-token"));
    let deps = deps(&http, &shared);
    for (channel_id, request_owner) in [
        (channel, UserId::new(4351)),
        (ChannelId::new(channel.get() + 1), UserId::new(4350)),
    ] {
        dispatch_skill_intake(
            &deps,
            ProviderKind::Claude,
            channel,
            MessageId::new(6_325_728),
            UserId::new(4350),
            "original owner".into(),
            "skill prompt".into(),
            IntakeOrigin::SlashSkill,
            Vec::new(),
            Some(super::super::LocalAdmissionPermit {
                channel_id,
                request_owner,
                prepared_uploads: Vec::new(),
            }),
        )
        .await
        .expect("mismatched local admission is consumed without executing");
        assert!(shared.core.lock().await.sessions.is_empty());
        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
        );
    }
}
